//! Comandos de red: `keygen`, `token`, `serve`, `bots`, `loadtest`.

use std::fs;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use tsofgw_online::bus::SimBusConfig;
use tsofgw_online::cluster_scenario::{
    run_cluster, run_cluster_chaos, ClusterReport, ClusterScenario, Invariants, Reconnect,
    Violations,
};
use tsofgw_online::cluster_udp::UdpCluster;
use tsofgw_online::partition::Partition;
use tsofgw_online::scenario::{run, Scenario};
use tsofgw_online::{
    default_lod, Bot, ClusterConfig, GameClient, GameConfig, GameServer, NewConnection,
    PersistConfig, RestoreReport, UdpBus,
};
use tsofgw_pack::{from_hex, to_hex};
use tsofgw_store::{FsMedium, OpenMode};
use tsofgw_transport::{
    Authority, ClientConfig, Entropy, OsEntropy, ServerConfig, SimConfig, UdpLink,
};

use crate::{flag, has, num, read_key};

/// Rechaza opciones que el comando no conoce (en vez de ignorarlas sin avisar).
pub fn reject_unknown(
    args: &[String],
    with_value: &[&str],
    switches: &[&str],
) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if with_value.contains(&a) {
            if i + 1 >= args.len() {
                return Err(format!("falta el valor de {a}"));
            }
            i += 2;
        } else if switches.contains(&a) {
            i += 1;
        } else if a.starts_with('-') {
            return Err(format!("opción desconocida: {a}"));
        } else {
            return Err(format!("argumento inesperado: {a}"));
        }
    }
    Ok(())
}

/// Lee una clave de un archivo (nunca de la línea de comandos: quedaría en el historial y en `ps`).
/// Se acepta hexadecimal (lo que escribe `keygen`) o cualquier texto de al menos 16 bytes.
pub(crate) fn key_from_file(path: &str, what: &str) -> Result<Vec<u8>, String> {
    let text = read_key(path)?;
    let key = std::str::from_utf8(&text)
        .ok()
        .and_then(|t| from_hex(t.trim()))
        .filter(|k| k.len() >= 16)
        .unwrap_or(text);
    if key.len() < 16 {
        return Err(format!("{what} debe tener al menos 16 bytes"));
    }
    Ok(key)
}

/// Los bytes de la clave maestra (de `--master-file`).
pub(crate) fn master_key(args: &[String]) -> Result<Vec<u8>, String> {
    let path = flag(args, "--master-file")
        .ok_or("falta --master-file (genera una con `tsofgw keygen`)")?;
    key_from_file(&path, "la clave maestra")
}

fn master(args: &[String]) -> Result<Authority, String> {
    Ok(Authority::new(&master_key(args)?))
}

/// `COLUMNASxFILAS` (por ejemplo `2x2`; un número solo es `Nx1`).
pub(crate) fn parse_grid(spec: &str, flag_name: &str) -> Result<(u16, u16), String> {
    let (a, b) = spec.split_once('x').unwrap_or((spec, "1"));
    let (cols, rows) = (
        a.parse::<u16>().map_err(|_| {
            format!("{flag_name} debe ser COLUMNASxFILAS (por ejemplo 2x2), no «{spec}»")
        })?,
        b.parse::<u16>().map_err(|_| {
            format!("{flag_name} debe ser COLUMNASxFILAS (por ejemplo 2x2), no «{spec}»")
        })?,
    );
    if cols == 0 || rows == 0 || cols as u32 * rows as u32 > 256 {
        return Err(format!("{flag_name}: entre 1 y 256 shards en total"));
    }
    Ok((cols, rows))
}

pub(crate) fn parse_addr_list(list: &str, flag_name: &str) -> Result<Vec<SocketAddr>, String> {
    list.split(',')
        .map(|a| resolve(a.trim()).map_err(|e| format!("{flag_name}: {e}")))
        .collect()
}

/// `tsofgw keygen -o master.key`: clave maestra aleatoria (32 bytes en hexadecimal). No sobrescribe.
pub fn cmd_keygen(args: &[String]) -> Result<(), String> {
    reject_unknown(args, &["-o"], &[])?;
    let out = flag(args, "-o").ok_or("falta -o <archivo>")?;
    let mut key = [0u8; 32];
    OsEntropy.fill(&mut key);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    let mut f = opts
        .open(&out)
        .map_err(|e| format!("no se pudo crear {out} (¿ya existe?): {e}"))?;
    writeln!(f, "{}", to_hex(&key)).map_err(|e| format!("no se pudo escribir {out}: {e}"))?;
    println!("clave escrita en {out} (32 bytes aleatorios). Guárdala en secreto: la clave maestra permite hacerse pasar por cualquier cuenta, y la de cluster por cualquier shard.");
    Ok(())
}

/// `tsofgw token --master-file K --player ID`: muestra la clave que corresponde a una cuenta.
pub fn cmd_token(args: &[String]) -> Result<(), String> {
    reject_unknown(args, &["--master-file", "--player"], &[])?;
    let auth = master(args)?;
    let id: u64 = num(args, "--player", 0)?;
    if id == 0 {
        return Err("falta --player <id> (número de cuenta, mayor que 0)".into());
    }
    let c = auth.issue(id);
    println!("cuenta: {}\nclave:  {}", c.player_id, to_hex(c.key_bytes()));
    Ok(())
}

pub(crate) fn game_config(args: &[String], default_threads: usize) -> Result<GameConfig, String> {
    let d = GameConfig::default();
    let cfg = GameConfig {
        threads: num(args, "--threads", default_threads)?,
        snapshot_budget: num(args, "--budget", d.snapshot_budget)?,
        lod: if has(args, "--lod") {
            default_lod()
        } else {
            d.lod.clone()
        },
        npc_count: num(args, "--npcs", d.npc_count)?,
        interest_radius: num(args, "--radius", d.interest_radius)?,
        sim_radius: num(args, "--sim-radius", d.sim_radius)?,
        tick_ms: num(args, "--tick-ms", d.tick_ms)?,
        seed: num(args, "--seed", d.seed)?,
        max_players: num(args, "--max-players", d.max_players)?,
        spawn_half_extent: num(args, "--spawn-half", d.spawn_half_extent)?,
        interest_share_min: num(args, "--share-min", d.interest_share_min)?,
        ..d
    };
    if !(10..=1000).contains(&cfg.tick_ms) {
        return Err("--tick-ms debe estar entre 10 y 1000".into());
    }
    if cfg.interest_radius <= 0.0 || cfg.sim_radius <= 0.0 {
        return Err("los radios deben ser positivos".into());
    }
    if cfg.npc_count > 5_000_000 {
        return Err("--npcs no puede superar 5 000 000".into());
    }
    if cfg.threads > 256 {
        return Err("--threads no puede superar 256 (0 = los núcleos disponibles)".into());
    }
    if !(1.0..=512.0).contains(&cfg.spawn_half_extent) {
        return Err("--spawn-half debe estar entre 1 y 512 (unidades; el mundo es de ±512)".into());
    }
    if cfg.snapshot_budget != 0 && !(64..=60_000).contains(&cfg.snapshot_budget) {
        return Err("--budget debe estar entre 64 y 60000 bytes (0 = sin límite)".into());
    }
    Ok(cfg)
}

pub(crate) fn resolve(addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("dirección inválida {addr}: {e}"))?
        .next()
        .ok_or(format!("no se pudo resolver {addr}"))
}

/// Aviso de seguridad si algún canal escucha fuera del bucle local: la red de esta versión no está cifrada y
/// no es apta para exponerse a internet (ver `SECURITY.md`). Devuelve `None` si todo es local.
pub(crate) fn exposure_warning(listening: &[(&str, SocketAddr)]) -> Option<String> {
    let exposed: Vec<String> = listening
        .iter()
        .filter(|(_, addr)| !addr.ip().is_loopback())
        .map(|(channel, addr)| format!("{channel} {addr}"))
        .collect();
    if exposed.is_empty() {
        return None;
    }
    Some(format!(
        "ADVERTENCIA: escuchando fuera del bucle local ({}). TSOFGW {} NO está pensado para exponerse a internet: \
         la red no está cifrada (ni la de jugadores ni el bus entre shards), quien tenga la clave de clúster puede \
         suplantar a cualquier shard, y el directorio de sesiones (si lo usas) es un solo proceso sin réplica. Úsalo en \
         una red privada o de confianza; detalles en SECURITY.md.",
        exposed.join(", "),
        env!("CARGO_PKG_VERSION")
    ))
}

/// Opciones de persistencia de `serve`.
struct PersistOpts {
    dir: String,
    mode: OpenMode,
    cfg: PersistConfig,
}

/// Opciones que solo valen con `--data-dir`.
const PERSIST_FLAGS: [&str; 4] = [
    "--sync-ticks",
    "--npc-save-ticks",
    "--player-save-ticks",
    "--compact-mb",
];

fn persist_opts(args: &[String]) -> Result<Option<PersistOpts>, String> {
    let Some(dir) = flag(args, "--data-dir") else {
        for f in PERSIST_FLAGS
            .iter()
            .copied()
            .chain(["--repair", "--directory"])
        {
            if flag(args, f).is_some() || has(args, f) {
                return Err(format!("{f} solo tiene sentido con --data-dir"));
            }
        }
        return Ok(None);
    };
    let d = PersistConfig::default();
    let compact_mb: u64 = num(args, "--compact-mb", d.compact_bytes >> 20)?;
    let cfg = PersistConfig {
        sync_ticks: num(args, "--sync-ticks", d.sync_ticks)?,
        npc_checkpoint_ticks: num(args, "--npc-save-ticks", d.npc_checkpoint_ticks)?,
        player_save_ticks: num(args, "--player-save-ticks", d.player_save_ticks)?,
        compact_bytes: compact_mb << 20,
        ..d
    };
    if cfg.sync_ticks == 0 || cfg.npc_checkpoint_ticks == 0 || cfg.player_save_ticks == 0 {
        return Err(
            "--sync-ticks, --npc-save-ticks y --player-save-ticks deben ser al menos 1".into(),
        );
    }
    if !(1..=4096).contains(&compact_mb) {
        return Err("--compact-mb debe estar entre 1 y 4096".into());
    }
    Ok(Some(PersistOpts {
        dir,
        mode: if has(args, "--repair") {
            OpenMode::Repair
        } else {
            OpenMode::Recover
        },
        cfg,
    }))
}

/// Abre la carpeta de datos (con su bloqueo) y lo que dice el almacén.
pub(crate) fn open_medium(dir: &str) -> Result<FsMedium, String> {
    FsMedium::open(dir).map_err(|e| format!("no se pudo abrir la carpeta de datos {dir}: {e}"))
}

/// Una línea para el operador sobre lo que encontró el arranque en la carpeta de datos.
fn describe_restore(dir: &str, r: &RestoreReport) -> String {
    let rec = &r.recovery;
    let mut o = if r.first_start {
        format!(
            "Persistencia en {dir}: primer arranque (mundo nuevo, generación {}).",
            rec.generation
        )
    } else {
        format!(
            "Persistencia en {dir}: restaurado — {} NPC, {} traspasos de NPC sin acuse, {} jugadores residentes, \
             {} traspasos de jugador sin acuse, {} lápidas (generación {}, {} registros del diario).",
            r.npcs, r.npc_transfers, r.residents, r.handovers, r.tombstones, rec.generation, rec.records
        )
    };
    if rec.torn_bytes > 0 {
        o.push_str(&format!(
            " Se descartó una cola rota de {} bytes (lo normal tras una caída).",
            rec.torn_bytes
        ));
    }
    if rec.repaired_bytes > 0 {
        o.push_str(&format!(
            "\nADVERTENCIA: --repair descartó {} bytes dañados en medio del diario; lo que venía después se perdió.",
            rec.repaired_bytes
        ));
    }
    o
}

/// `tsofgw serve`: servidor real sobre UDP. Se detiene con Ctrl-C (o `SIGTERM`) o tras `--duration`
/// segundos, y en los dos casos cierra el almacén con orden y avisa a los clientes.
pub fn cmd_serve(args: &[String]) -> Result<(), String> {
    reject_unknown(
        args,
        &[
            "--master-file",
            "--bind",
            "--npcs",
            "--radius",
            "--sim-radius",
            "--tick-ms",
            "--seed",
            "--max-players",
            "--duration",
            "--threads",
            "--budget",
            "--spawn-half",
            "--share-min",
            "--shard",
            "--grid",
            "--peers",
            "--bus-bind",
            "--cluster-key-file",
            "--data-dir",
            "--directory",
            "--sync-ticks",
            "--npc-save-ticks",
            "--player-save-ticks",
            "--compact-mb",
        ],
        &["--lod", "--repair"],
    )?;
    let auth = master(args)?;
    let bind = flag(args, "--bind").unwrap_or_else(|| "0.0.0.0:7777".into());
    // En un servidor real, por defecto se usan los núcleos disponibles.
    let cfg = game_config(args, 0)?;
    let duration: u64 = num(args, "--duration", 0)?;
    let persist = persist_opts(args)?;
    if flag(args, "--directory").is_some() && flag(args, "--shard").is_none() {
        return Err("--directory solo tiene sentido con --shard".into());
    }
    let link = UdpLink::bind(&bind).map_err(|e| format!("no se pudo abrir {bind}: {e}"))?;
    let local = link.local_addr().map_err(|e| e.to_string())?;
    let tick_ms = cfg.tick_ms;
    let mut listening = vec![("jugadores", local)];
    // El secreto de donde sale la clave de integridad del almacén: la de cluster en un shard (todos
    // los shards la comparten y cada uno deriva la suya), la maestra en un servidor suelto.
    let (mut server, store_secret) = match flag(args, "--shard") {
        None => {
            for f in ["--grid", "--peers", "--bus-bind", "--cluster-key-file"] {
                if flag(args, f).is_some() {
                    return Err(format!("{f} solo tiene sentido con --shard"));
                }
            }
            let secret = if persist.is_some() {
                master_key(args)?
            } else {
                Vec::new()
            };
            (
                GameServer::new(
                    link,
                    auth,
                    cfg,
                    ServerConfig::default(),
                    Box::new(OsEntropy),
                ),
                secret,
            )
        }
        Some(_) => {
            let boot = serve_shard(args, link, auth, cfg)?;
            listening.push(("bus entre shards", boot.bus_addr));
            (boot.server, boot.cluster_key)
        }
    };
    let restore = match &persist {
        Some(p) => {
            let medium = open_medium(&p.dir)?;
            let key = server.store_key(&store_secret);
            let rep = server.enable_persistence(Box::new(medium), &key, p.cfg.clone(), p.mode)?;
            Some((p.dir.clone(), rep))
        }
        None => None,
    };
    let cluster_note = server
        .shard_id()
        .map(|s| {
            format!(
                ", shard {s} de {}",
                server
                    .cluster_config()
                    .map_or(1, |c| c.partition.shard_count())
            )
        })
        .unwrap_or_default();
    println!(
        "TSOFGW servidor escuchando en {local} ({} NPC{cluster_note}, tick {tick_ms} ms, {} hilo(s)). Ctrl-C para detener.",
        server.entity_count(),
        tsofgw_online::par::resolve_threads(server.config().threads)
    );
    if let Some((dir, rep)) = &restore {
        println!("{}", describe_restore(dir, rep));
    }
    if let Some(aviso) = exposure_warning(&listening) {
        eprintln!("{aviso}");
    }
    tsofgw_sys::stop::install();
    let start = Instant::now();
    let mut last_report = 0u64;
    let mut warned_store = false;
    loop {
        let now = start.elapsed().as_millis() as u64;
        server.update(now);
        if now - last_report >= 5000 {
            last_report = now;
            let g = server.stats();
            let avg = if g.ticks > 0 {
                g.tick_time_us as f64 / g.ticks as f64
            } else {
                0.0
            };
            let sim = if g.npcs_total > 0 {
                g.npcs_simulated as f64 / g.npcs_total as f64 * 100.0
            } else {
                0.0
            };
            println!(
                "[{:>5} s] jugadores {} | ticks {} (media {:.0} µs, máx {} µs) | NPC simulados {:.1} % | expulsados {} | etiquetas inválidas {}",
                now / 1000, server.player_count(), g.ticks, avg, g.tick_time_max_us, sim, g.kicks,
                server.transport_stats().bad_tag
            );
            if let Some(c) = server.cluster_stats() {
                println!(
                    "          cluster: NPC propios {} | copias {} | traspasos de jugador {}/{} (cancelados {}, llegadas {}) | NPC traspasados {}→ / ←{} | bus: rechazados {}, mal formados {}",
                    server.owned_npc_count(),
                    server.ghost_count(),
                    c.handoffs_completed,
                    c.handoffs_started,
                    c.handoffs_aborted,
                    c.arrivals_completed,
                    c.npc_xfers_out,
                    c.npc_xfers_in,
                    c.bus_rejected,
                    c.bus_malformed
                );
                if server.cluster_config().is_some_and(|c| c.directory) {
                    println!(
                        "          sesiones: entradas redirigidas {} | esperando al directorio {} | traspasos sin acuse {}",
                        c.login_redirects,
                        c.login_waits,
                        server.handover_outbox().len()
                    );
                }
            }
            if let Some(st) = server.persist_stats() {
                println!(
                    "          almacén: commits {} | NPC guardados {} | jugadores guardados {} | compactaciones {} | diario {:.1} KB | promesas retenidas {}",
                    st.commits,
                    st.npc_saves,
                    st.player_saves,
                    st.compactions,
                    server.store_log_bytes().unwrap_or(0) as f64 / 1000.0,
                    st.deferred_dropped
                );
            }
        }
        if server.persistence_failed() && !warned_store {
            warned_store = true;
            eprintln!(
                "ERROR: el almacén falló al escribir. Este servidor sigue jugando en memoria pero ya no promete nada \
                 (no entrega ni acepta cuentas ni NPC de forma duradera). Lo ya sincronizado está a salvo: revisa el \
                 disco y reinícialo."
            );
        }
        if tsofgw_sys::stop::requested() {
            println!("Parada pedida: cerrando el servidor...");
            break;
        }
        if duration > 0 && now >= duration * 1000 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let persistent = server.persistence_enabled();
    server.shutdown()?;
    if persistent {
        println!("Cierre limpio: estado guardado, almacén compactado y clientes avisados.");
    }
    Ok(())
}

/// Un shard listo para arrancar y lo que hace falta saber de él.
struct ShardBoot {
    server: GameServer<UdpLink>,
    bus_addr: SocketAddr,
    /// La clave de cluster, de donde sale la clave de integridad de su almacén.
    cluster_key: Vec<u8>,
}

/// Un shard de un cluster: lee la rejilla, las direcciones de los demás shards y la clave del cluster.
fn serve_shard(
    args: &[String],
    link: UdpLink,
    auth: Authority,
    cfg: GameConfig,
) -> Result<ShardBoot, String> {
    let shard: u16 = num(args, "--shard", 0)?;
    let (cols, rows) = parse_grid(
        &flag(args, "--grid")
            .ok_or("falta --grid COLUMNASxFILAS (la misma en todos los shards)")?,
        "--grid",
    )?;
    let peers = parse_addr_list(
        &flag(args, "--peers")
            .ok_or("falta --peers DIR0,DIR1,... (el bus de cada shard, en orden)")?,
        "--peers",
    )?;
    let key_path = flag(args, "--cluster-key-file").ok_or(
        "falta --cluster-key-file (genera una con `tsofgw keygen`; la misma en todos los shards)",
    )?;
    let key = key_from_file(&key_path, "la clave de cluster")?;
    let partition = Partition::world_grid(cols, rows)?;
    let n = partition.shard_count();
    if peers.len() != n {
        return Err(format!(
            "--peers debe tener {n} direcciones (una por shard, incluido este) y tiene {}",
            peers.len()
        ));
    }
    if shard as usize >= n {
        return Err(format!(
            "--shard {shard}: la rejilla {cols}x{rows} tiene {n} shards (0..{})",
            n - 1
        ));
    }
    let directory = flag(args, "--directory")
        .map(|a| resolve(&a).map_err(|e| format!("--directory: {e}")))
        .transpose()?;
    let bus_bind = flag(args, "--bus-bind").unwrap_or_else(|| peers[shard as usize].to_string());
    // Con directorio, el bus tiene un par más: el directorio, con el identificador siguiente al último shard.
    let mut bus = UdpBus::bind(shard, &bus_bind, n + usize::from(directory.is_some()), &key)?;
    for (j, a) in peers.iter().enumerate() {
        if j != shard as usize {
            bus.set_peer(j as u16, *a)?;
        }
    }
    if let Some(a) = directory {
        bus.set_peer(n as u16, a)?;
    }
    let mut cluster = ClusterConfig::new(shard, partition, &cfg)?;
    cluster.directory = directory.is_some();
    let bus_addr = bus
        .local_addr()
        .map_err(|e| format!("no se pudo leer la dirección del bus: {e}"))?;
    let server = GameServer::new_shard(
        link,
        auth,
        cfg,
        ServerConfig::default(),
        Box::new(OsEntropy),
        cluster,
        Box::new(bus),
    )?;
    Ok(ShardBoot {
        server,
        bus_addr,
        cluster_key: key,
    })
}

/// `tsofgw bots`: bots reales que se conectan por UDP a un servidor.
pub fn cmd_bots(args: &[String]) -> Result<(), String> {
    reject_unknown(
        args,
        &[
            "--master-file",
            "--connect",
            "--count",
            "--first-id",
            "--duration",
            "--shards",
            "--patrol",
            "--patrol-ms",
        ],
        &[],
    )?;
    let auth = master(args)?;
    let server = resolve(&flag(args, "--connect").ok_or("falta --connect HOST:PUERTO")?)?;
    // En un cluster, la dirección de cada shard (en orden) para seguir las redirecciones entre ellos.
    let shard_addrs: Vec<SocketAddr> = match flag(args, "--shards") {
        Some(l) => parse_addr_list(&l, "--shards")?,
        None => Vec::new(),
    };
    let patrol: u64 = num(args, "--patrol", 0)?;
    let patrol_ms: u64 = num(args, "--patrol-ms", 8000)?;
    let count: u64 = num(args, "--count", 10)?;
    let first: u64 = num(args, "--first-id", 1000)?;
    let duration: u64 = num(args, "--duration", 30)?;
    if count == 0 || count > 5000 {
        return Err("--count debe estar entre 1 y 5000".into());
    }
    let bind = if server.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let mut bots: Vec<Bot<UdpLink>> = (0..count)
        .map(|i| {
            let link =
                UdpLink::bind(bind).map_err(|e| format!("no se pudo abrir un socket: {e}"))?;
            let mut client = GameClient::new(
                link,
                server,
                auth.issue(first + i),
                ClientConfig::default(),
                Box::new(OsEntropy),
            );
            if !shard_addrs.is_empty() {
                let addrs = shard_addrs.clone();
                client = client.with_connector(Box::new(move |shard| {
                    let addr = *addrs.get(shard as usize)?;
                    let bind = if addr.is_ipv4() {
                        "0.0.0.0:0"
                    } else {
                        "[::]:0"
                    };
                    Some(NewConnection {
                        link: UdpLink::bind(bind).ok()?,
                        server: addr,
                        entropy: Box::new(OsEntropy),
                    })
                }));
            }
            let mut bot = Bot::new(client, first + i);
            if i < patrol {
                // Camina en línea recta y vuelve, para cruzar bordes entre shards una y otra vez.
                let dir = [[127, 0, 0], [0, 0, 127], [90, 0, 90]][(i % 3) as usize];
                bot = bot.with_patrol(dir, patrol_ms);
            }
            Ok(bot)
        })
        .collect::<Result<_, String>>()?;
    println!("{count} bots conectando a {server} durante {duration} s...");
    let start = Instant::now();
    let mut last = 0u64;
    loop {
        let now = start.elapsed().as_millis() as u64;
        for b in &mut bots {
            b.update(now);
        }
        if now - last >= 5000 {
            last = now;
            let playing = bots.iter().filter(|b| b.client.is_playing()).count();
            let rtts: Vec<f64> = bots.iter().filter_map(|b| b.client.rtt_ms()).collect();
            let rtt = if rtts.is_empty() {
                0.0
            } else {
                rtts.iter().sum::<f64>() / rtts.len() as f64
            };
            let switches: u64 = bots.iter().map(|b| b.client.stats().switches).sum();
            let failed: u64 = bots.iter().map(|b| b.client.stats().switch_failures).sum();
            let entradas: u64 = bots.iter().map(|b| b.client.stats().login_redirects).sum();
            let extra = if shard_addrs.is_empty() {
                String::new()
            } else {
                // Los cambios de shard son los traspasos en pleno juego; las entradas redirigidas, las veces
                // que al pedir entrar el servidor mandó al cliente al shard de su cuenta.
                format!(
                    " | cambios de shard {switches} ({failed} fallidos) | entradas redirigidas {entradas}"
                )
            };
            println!(
                "[{:>4} s] jugando {playing}/{count} | RTT medio {rtt:.1} ms{extra}",
                now / 1000
            );
        }
        if now >= duration * 1000 {
            for b in &mut bots {
                b.client.disconnect();
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `tsofgw loadtest`: servidor y bots en un mismo proceso. Por defecto sobre una red simulada y
/// determinista; con `--udp`, sobre sockets UDP reales de este equipo en tiempo real.
pub fn cmd_loadtest(args: &[String]) -> Result<(), String> {
    reject_unknown(
        args,
        &[
            "--clients",
            "--npcs",
            "--seconds",
            "--loss",
            "--latency",
            "--jitter",
            "--dup",
            "--seed",
            "--radius",
            "--sim-radius",
            "--tick-ms",
            "--max-players",
            "--threads",
            "--budget",
            "--spawn-half",
            "--share-min",
            "--shards",
            "--bus-loss",
            "--bus-latency",
            "--bus-jitter",
            "--patrol",
            "--patrol-ms",
            "--chaos",
            "--chaos-seed",
        ],
        &["--udp", "--lod", "--persist", "--directory"],
    )?;
    let mut game = game_config(args, 1)?;
    let clients: usize = num(args, "--clients", 100)?;
    let seconds: u64 = num(args, "--seconds", 20)?;
    if clients > 5000 {
        return Err("--clients no puede superar 5000".into());
    }
    if !(2..=600).contains(&seconds) {
        return Err("--seconds debe estar entre 2 y 600".into());
    }
    game.max_players = game.max_players.max(clients);
    let loss: f64 = num(args, "--loss", 0.02)?;
    let dup: f64 = num(args, "--dup", 0.0)?;
    if !(0.0..=0.9).contains(&loss) || !(0.0..=0.9).contains(&dup) {
        return Err("--loss y --dup deben estar entre 0 y 0.9".into());
    }
    if let Some(spec) = flag(args, "--shards") {
        return loadtest_cluster(args, &spec, clients, seconds, game, loss, dup);
    }
    for f in ["--persist", "--directory", "--chaos", "--chaos-seed"] {
        if flag(args, f).is_some() || has(args, f) {
            return Err(format!("{f} solo tiene sentido con --shards"));
        }
    }
    if has(args, "--udp") {
        return loadtest_udp(clients, seconds, game);
    }
    let s = Scenario {
        clients,
        duration_ms: seconds * 1000,
        net: SimConfig {
            latency_ms: num(args, "--latency", 30)?,
            jitter_ms: num(args, "--jitter", 10)?,
            loss,
            duplicate: dup,
            seed: game.seed,
        },
        game,
        ..Scenario::default()
    };
    print!("{}", run(&s).render());
    Ok(())
}

/// `loadtest --shards CxF`: un cluster de shards en este proceso, con clientes que cruzan los bordes.
fn loadtest_cluster(
    args: &[String],
    spec: &str,
    clients: usize,
    seconds: u64,
    mut game: GameConfig,
    loss: f64,
    dup: f64,
) -> Result<(), String> {
    let (cols, rows) = parse_grid(spec, "--shards")?;
    // Para ver los bordes en acción, por defecto los jugadores aparecen cerca del cruce de las regiones.
    if flag(args, "--spawn-half").is_none() {
        game.spawn_half_extent = 40.0;
    }
    let patrollers: usize = num(args, "--patrol", clients / 2)?;
    let bus_loss: f32 = num(args, "--bus-loss", 0.02)?;
    if !(0.0..=0.9).contains(&bus_loss) {
        return Err("--bus-loss debe estar entre 0 y 0.9".into());
    }
    // Persistencia en los shards (en memoria, con caídas inyectables), directorio de sesiones y una
    // tormenta de caídas: cada nivel exige el anterior y todos exigen la red simulada.
    let chaos: usize = num(args, "--chaos", 0)?;
    let directory = has(args, "--directory");
    let persist = has(args, "--persist") || directory || chaos > 0;
    if !(0..=500).contains(&chaos) {
        return Err("--chaos debe estar entre 0 y 500 rondas".into());
    }
    if flag(args, "--chaos-seed").is_some() && chaos == 0 {
        return Err("--chaos-seed solo tiene sentido con --chaos".into());
    }
    if persist && has(args, "--udp") {
        return Err(
            "--persist, --directory y --chaos usan la red simulada: no se combinan con --udp"
                .into(),
        );
    }
    let s = ClusterScenario {
        grid: (cols, rows),
        clients,
        patrollers: patrollers.min(clients),
        patrol_ms: num(args, "--patrol-ms", 8000)?,
        duration_ms: seconds * 1000,
        net: SimConfig {
            latency_ms: num(args, "--latency", 30)?,
            jitter_ms: num(args, "--jitter", 10)?,
            loss,
            duplicate: dup,
            seed: game.seed,
        },
        bus: SimBusConfig {
            latency_ms: num(args, "--bus-latency", 10)?,
            jitter_ms: num(args, "--bus-jitter", 5)?,
            loss: bus_loss,
            duplicate: 0.0,
            seed: game.seed ^ 0x5eed,
        },
        game,
        persist: persist.then(PersistConfig::default),
        directory,
        // Con persistencia los clientes que pierden la conexión vuelven a entrar solos, como en un juego
        // real; con directorio entran por cualquier shard y el cluster los lleva al suyo.
        reconnect: Reconnect {
            enabled: persist,
            delay_ms: 500,
            any_shard: directory,
        },
        ..ClusterScenario::default()
    };
    if has(args, "--udp") {
        return loadtest_cluster_udp(&s);
    }
    let r = if chaos > 0 {
        run_cluster_chaos(&s, chaos, num(args, "--chaos-seed", 1u64)?)?
    } else {
        run_cluster(&s)?
    };
    print!("{}", r.render());
    cluster_report_verdict(&r, s.clients)
}

/// El veredicto de un cluster con persistencia: además del reparto final, durante toda la marcha no debe
/// haberse visto ninguna cuenta duplicada, simulada por dos shards, perdida ni en desacuerdo con el
/// directorio.
fn cluster_report_verdict(r: &ClusterReport, clients: usize) -> Result<(), String> {
    cluster_verdict(&r.invariants, clients)?;
    violations_verdict(&r.violations)?;
    if r.persistent
        && r.shards
            .iter()
            .any(|s| s.store.as_ref().is_some_and(|st| st.failed))
    {
        return Err("el almacén de algún shard falló durante la prueba".into());
    }
    Ok(())
}

fn violations_verdict(v: &Violations) -> Result<(), String> {
    if *v == Violations::default() {
        return Ok(());
    }
    Err(format!(
        "se violó un invariante durante la marcha: cuentas residentes en dos shards {}, simuladas por dos shards a \
         la vez {}, cuentas desaparecidas {}, residentes que el directorio sitúa en otro shard {}, NPC sin dueño {}",
        v.resident_duplicates, v.simulated_twice, v.lost_accounts, v.dir_mismatch, v.npc_lost
    ))
}

/// El reparto de un cluster debe quedar perfecto al terminar: nada perdido ni duplicado y todos jugando.
/// Si no, el comando falla, de modo que sirve como prueba de humo en la CI.
fn cluster_verdict(inv: &Invariants, clients: usize) -> Result<(), String> {
    if inv.is_exact() && inv.players_playing == clients {
        return Ok(());
    }
    Err(format!(
        "el cluster no quedó exacto: NPC perdidos {}, duplicados {}; jugadores perdidos {}, duplicados {}; \
         jugando {}/{}",
        inv.npc_lost,
        inv.npc_duplicated,
        inv.players_lost,
        inv.players_duplicated,
        inv.players_playing,
        clients
    ))
}

/// El mismo escenario sobre sockets UDP reales de este equipo (shards, bus entre shards y clientes), en
/// tiempo real. La red es la del equipo: `--loss`, `--latency` y `--bus-*` no se aplican.
fn loadtest_cluster_udp(s: &ClusterScenario) -> Result<(), String> {
    let mut key = [0u8; 32];
    OsEntropy.fill(&mut key);
    let mut cluster = UdpCluster::new(s, &key)?;
    println!(
        "Cluster {}x{} sobre UDP real por loopback: {} clientes, {} NPC, {} s en tiempo real...",
        s.grid.0,
        s.grid.1,
        s.clients,
        s.game.npc_count,
        s.duration_ms / 1000
    );
    let start = Instant::now();
    let mut last = 0u64;
    loop {
        let now = start.elapsed().as_millis() as u64;
        cluster.update(now);
        if now >= s.duration_ms {
            break;
        }
        if now - last >= 5000 {
            last = now;
            let inv = cluster.check(false);
            println!(
                "[{:>4} s] jugando {}/{} | NPC perdidos {} | jugadores perdidos {}",
                now / 1000,
                inv.players_playing,
                s.clients,
                inv.npc_lost,
                inv.players_lost
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // Los bots se detienen y se deja un tiempo para que todo converja antes de comprobar el reparto.
    cluster.settle();
    let until = cluster.now + s.settle_ms;
    while cluster.now < until {
        let now = start.elapsed().as_millis() as u64;
        cluster.update(now);
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut r = cluster.report();
    r.invariants = cluster.check(true);
    print!("{}", r.render());
    cluster_verdict(&r.invariants, s.clients)
}

fn loadtest_udp(clients: usize, seconds: u64, game: GameConfig) -> Result<(), String> {
    let auth = Authority::new(b"loadtest-udp-master-key");
    let link = UdpLink::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let addr = link.local_addr().map_err(|e| e.to_string())?;
    let npcs = game.npc_count;
    let mut server = GameServer::new(
        link,
        auth.clone(),
        game,
        ServerConfig::default(),
        Box::new(OsEntropy),
    );
    let mut bots: Vec<Bot<UdpLink>> = (0..clients)
        .map(|i| {
            let link = UdpLink::bind("127.0.0.1:0").map_err(|e| format!("socket: {e}"))?;
            Ok(Bot::new(
                GameClient::new(
                    link,
                    addr,
                    auth.issue(1000 + i as u64),
                    ClientConfig::default(),
                    Box::new(OsEntropy),
                ),
                i as u64 + 1,
            ))
        })
        .collect::<Result<_, String>>()?;
    println!(
        "UDP real por loopback ({addr}): {clients} bots, {npcs} NPC, {seconds} s en tiempo real..."
    );
    let start = Instant::now();
    loop {
        let now = start.elapsed().as_millis() as u64;
        server.update(now);
        for b in &mut bots {
            b.update(now);
        }
        if now >= seconds * 1000 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let g = server.stats();
    let t = server.transport_stats();
    let playing = bots.iter().filter(|b| b.client.is_playing()).count();
    let n = playing.max(1) as f64;
    let secs = seconds as f64;
    let rtts: Vec<f64> = bots.iter().filter_map(|b| b.client.rtt_ms()).collect();
    let rtt = if rtts.is_empty() {
        0.0
    } else {
        rtts.iter().sum::<f64>() / rtts.len() as f64
    };
    let (cn, cs): (u64, u64) = bots
        .iter()
        .map(|b| {
            (
                b.client.stats().corrections,
                b.client.stats().correction_sum_q,
            )
        })
        .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    println!(
        "jugando: {playing}/{clients}\n\
         servidor: {} ticks, media {:.0} µs, máximo {} µs (presupuesto {} µs)\n\
         NPC simulados: {:.1} % del total\n\
         snapshots: {} completos + {} delta\n\
         bajada: {:.1} kbps por cliente | subida: {:.1} kbps por cliente (carga UDP, incluye el arranque)\n\
         RTT medio {:.2} ms | correcciones de predicción: {} (media {:.2} u)\n\
         paquetes rechazados: etiqueta {} / repetidos {} / mal formados {}",
        g.ticks,
        g.tick_time_us as f64 / g.ticks.max(1) as f64,
        g.tick_time_max_us,
        server.config().tick_ms * 1000,
        g.npcs_simulated as f64 / g.npcs_total.max(1) as f64 * 100.0,
        g.snapshots_full,
        g.snapshots_delta,
        t.bytes_out as f64 * 8.0 / secs / n / 1000.0,
        bots.iter().map(|b| b.client.transport_stats().bytes_out).sum::<u64>() as f64 * 8.0 / secs / n / 1000.0,
        rtt,
        cn,
        if cn > 0 { cs as f64 / cn as f64 / 64.0 } else { 0.0 },
        t.bad_tag,
        t.replays,
        t.malformed + g.malformed,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avisa_solo_de_lo_que_escucha_fuera_del_bucle_local() {
        let local4: SocketAddr = "127.0.0.1:7777".parse().unwrap();
        let local6: SocketAddr = "[::1]:7777".parse().unwrap();
        assert!(exposure_warning(&[("jugadores", local4), ("bus entre shards", local6)]).is_none());
        assert!(exposure_warning(&[]).is_none());

        let todas: SocketAddr = "0.0.0.0:7777".parse().unwrap();
        let aviso = exposure_warning(&[("jugadores", todas)]).expect("0.0.0.0 es exponer");
        assert!(aviso.contains("jugadores 0.0.0.0:7777"), "{aviso}");
        assert!(aviso.contains("NO está pensado para exponerse a internet"));
        assert!(aviso.contains("SECURITY.md"));

        // Solo se nombra el canal expuesto.
        let privada: SocketAddr = "192.168.1.5:7000".parse().unwrap();
        let aviso =
            exposure_warning(&[("jugadores", local4), ("bus entre shards", privada)]).unwrap();
        assert!(
            aviso.contains("bus entre shards 192.168.1.5:7000"),
            "{aviso}"
        );
        assert!(!aviso.contains("jugadores 127.0.0.1"), "{aviso}");
    }

    fn exact(clients: usize) -> Invariants {
        Invariants {
            npcs: 100,
            players_playing: clients,
            ..Invariants::default()
        }
    }

    #[test]
    fn el_veredicto_del_cluster_acepta_solo_un_reparto_perfecto() {
        assert!(cluster_verdict(&exact(20), 20).is_ok());

        let lost = Invariants {
            npc_lost: 2,
            ..exact(20)
        };
        let e = cluster_verdict(&lost, 20).unwrap_err();
        assert!(e.contains("NPC perdidos 2"), "{e}");

        let dup = Invariants {
            players_duplicated: 1,
            ..exact(20)
        };
        assert!(cluster_verdict(&dup, 20)
            .unwrap_err()
            .contains("jugadores perdidos 0, duplicados 1"));

        // Todo exacto pero faltan jugadores jugando: tampoco es un resultado válido.
        let e = cluster_verdict(&exact(18), 20).unwrap_err();
        assert!(e.contains("jugando 18/20"), "{e}");
    }

    #[test]
    fn una_violacion_durante_la_marcha_basta_para_suspender() {
        assert!(violations_verdict(&Violations::default()).is_ok());
        let v = Violations {
            simulated_twice: 1,
            ..Violations::default()
        };
        let e = violations_verdict(&v).unwrap_err();
        assert!(e.contains("simuladas por dos shards a la vez 1"), "{e}");
    }
}
