//! Comandos de red: `keygen`, `token`, `serve`, `bots`, `loadtest`.

use std::fs;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use tsofgw_online::scenario::{run, Scenario};
use tsofgw_online::{Bot, GameClient, GameConfig, GameServer};
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

fn master(args: &[String]) -> Result<Authority, String> {
    let path = flag(args, "--master-file")
        .ok_or("falta --master-file (genera una con `tsofgw keygen`)")?;
    let text = read_key(&path)?;
    // Se acepta hexadecimal (lo que escribe keygen) o cualquier texto como clave.
    let key = std::str::from_utf8(&text)
        .ok()
        .and_then(|t| from_hex(t.trim()))
        .filter(|k| k.len() >= 16)
        .unwrap_or(text);
    if key.len() < 16 {
        return Err("la clave maestra debe tener al menos 16 bytes".into());
    }
    Ok(Authority::new(&key))
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
    println!("clave maestra escrita en {out}. Guárdala en secreto: quien la tenga puede hacerse pasar por cualquier cuenta.");
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

fn game_config(args: &[String]) -> Result<GameConfig, String> {
    let d = GameConfig::default();
    let cfg = GameConfig {
        npc_count: num(args, "--npcs", d.npc_count)?,
        interest_radius: num(args, "--radius", d.interest_radius)?,
        sim_radius: num(args, "--sim-radius", d.sim_radius)?,
        tick_ms: num(args, "--tick-ms", d.tick_ms)?,
        seed: num(args, "--seed", d.seed)?,
        max_players: num(args, "--max-players", d.max_players)?,
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
    Ok(cfg)
}

fn resolve(addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("dirección inválida {addr}: {e}"))?
        .next()
        .ok_or(format!("no se pudo resolver {addr}"))
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
        ],
        &[],
    )?;
    let auth = master(args)?;
    let bind = flag(args, "--bind").unwrap_or_else(|| "0.0.0.0:7777".into());
    let cfg = game_config(args)?;
    let duration: u64 = num(args, "--duration", 0)?;
    let link = UdpLink::bind(&bind).map_err(|e| format!("no se pudo abrir {bind}: {e}"))?;
    let local = link.local_addr().map_err(|e| e.to_string())?;
    let tick_ms = cfg.tick_ms;
    let mut server = GameServer::new(
        link,
        auth,
        cfg,
        ServerConfig::default(),
        Box::new(OsEntropy),
    );
    println!(
        "TSOFGW servidor escuchando en {local} ({} NPC, tick {tick_ms} ms). Ctrl-C para detener.",
        server.entity_count()
    );
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
        }
        if duration > 0 && now >= duration * 1000 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
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
        ],
        &[],
    )?;
    let auth = master(args)?;
    let server = resolve(&flag(args, "--connect").ok_or("falta --connect HOST:PUERTO")?)?;
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
            let client = GameClient::new(
                link,
                server,
                auth.issue(first + i),
                ClientConfig::default(),
                Box::new(OsEntropy),
            );
            Ok(Bot::new(client, first + i))
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
            println!(
                "[{:>4} s] jugando {playing}/{count} | RTT medio {rtt:.1} ms",
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
        ],
        &["--udp"],
    )?;
    let mut game = game_config(args)?;
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
