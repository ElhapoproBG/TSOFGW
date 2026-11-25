//! Comandos de red: `keygen`, `token`, `serve`, `bots`, `loadtest`.

use std::fs;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use tsofgw_online::bus::SimBusConfig;
use tsofgw_online::cluster_scenario::{run_cluster, ClusterScenario, Invariants};
use tsofgw_online::cluster_udp::UdpCluster;
use tsofgw_online::partition::Partition;
use tsofgw_online::scenario::{run, Scenario};
use tsofgw_online::{
    default_lod, Bot, ClusterConfig, GameClient, GameConfig, GameServer, NewConnection, UdpBus,
};
use tsofgw_pack::{from_hex, to_hex};
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
fn key_from_file(path: &str, what: &str) -> Result<Vec<u8>, String> {
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

fn master(args: &[String]) -> Result<Authority, String> {
    let path = flag(args, "--master-file")
        .ok_or("falta --master-file (genera una con `tsofgw keygen`)")?;
    Ok(Authority::new(&key_from_file(&path, "la clave maestra")?))
}

/// `COLUMNASxFILAS` (por ejemplo `2x2`; un número solo es `Nx1`).
fn parse_grid(spec: &str, flag_name: &str) -> Result<(u16, u16), String> {
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

fn parse_addr_list(list: &str, flag_name: &str) -> Result<Vec<SocketAddr>, String> {
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

fn game_config(args: &[String], default_threads: usize) -> Result<GameConfig, String> {
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

fn resolve(addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("dirección inválida {addr}: {e}"))?
        .next()
        .ok_or(format!("no se pudo resolver {addr}"))
}

/// Aviso de seguridad si algún canal escucha fuera del bucle local: la red de esta versión no está cifrada y
/// no es apta para exponerse a internet (ver `SECURITY.md`). Devuelve `None` si todo es local.
fn exposure_warning(listening: &[(&str, SocketAddr)]) -> Option<String> {
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
         suplantar a cualquier shard y no hay persistencia ni directorio de sesiones. Úsalo en una red privada o \
         de confianza; detalles en SECURITY.md.",
        exposed.join(", "),
        env!("CARGO_PKG_VERSION")
    ))
}

/// `tsofgw serve`: servidor real sobre UDP. Se detiene con Ctrl-C o tras `--duration` segundos.
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
        ],
        &["--lod"],
    )?;
    let auth = master(args)?;
    let bind = flag(args, "--bind").unwrap_or_else(|| "0.0.0.0:7777".into());
    // En un servidor real, por defecto se usan los núcleos disponibles.
    let cfg = game_config(args, 0)?;
    let duration: u64 = num(args, "--duration", 0)?;
    let link = UdpLink::bind(&bind).map_err(|e| format!("no se pudo abrir {bind}: {e}"))?;
    let local = link.local_addr().map_err(|e| e.to_string())?;
    let tick_ms = cfg.tick_ms;
    let mut listening = vec![("jugadores", local)];
    let mut server = match flag(args, "--shard") {
        None => {
            for f in ["--grid", "--peers", "--bus-bind", "--cluster-key-file"] {
                if flag(args, f).is_some() {
                    return Err(format!("{f} solo tiene sentido con --shard"));
                }
            }
            GameServer::new(
                link,
                auth,
                cfg,
                ServerConfig::default(),
                Box::new(OsEntropy),
            )
        }
        Some(_) => {
            let (server, bus_addr) = serve_shard(args, link, auth, cfg)?;
            listening.push(("bus entre shards", bus_addr));
            server
        }
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
    if let Some(aviso) = exposure_warning(&listening) {
        eprintln!("{aviso}");
    }
    let start = Instant::now();
    let mut last_report = 0u64;
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
            }
        }
        if duration > 0 && now >= duration * 1000 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Un shard de un cluster: lee la rejilla, las direcciones de los demás shards y la clave del cluster.
fn serve_shard(
    args: &[String],
    link: UdpLink,
    auth: Authority,
    cfg: GameConfig,
) -> Result<(GameServer<UdpLink>, SocketAddr), String> {
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
    let bus_bind = flag(args, "--bus-bind").unwrap_or_else(|| peers[shard as usize].to_string());
    let mut bus = UdpBus::bind(shard, &bus_bind, n, &key)?;
    for (j, a) in peers.iter().enumerate() {
        if j != shard as usize {
            bus.set_peer(j as u16, *a)?;
        }
    }
    let cluster = ClusterConfig::new(shard, partition, &cfg)?;
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
    Ok((server, bus_addr))
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
            let extra = if shard_addrs.is_empty() {
                String::new()
            } else {
                format!(" | cambios de shard {switches} ({failed} fallidos)")
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
        ],
        &["--udp", "--lod"],
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
        ..ClusterScenario::default()
    };
    if has(args, "--udp") {
        return loadtest_cluster_udp(&s);
    }
    let r = run_cluster(&s)?;
    print!("{}", r.render());
    cluster_verdict(&r.invariants, s.clients)
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
}
