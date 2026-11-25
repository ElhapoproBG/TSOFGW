//! Banco de pruebas: un servidor y N bots sobre una red simulada, con un informe medido.
//!
//! Todo es determinista salvo los tiempos de CPU (que miden el hardware real). Sirve para las pruebas
//! automáticas y para el comando `tsofgw loadtest`.

use std::time::Instant;

use tsofgw_transport::{
    Authority, ClientConfig, DeterministicEntropy, ServerConfig, SimConfig, SimLink, SimNet,
};

use crate::{Bot, GameClient, GameConfig, GameServer};

#[derive(Debug, Clone)]
pub struct Scenario {
    pub clients: usize,
    /// Duración del tramo en el que los bots se mueven (ms de tiempo simulado).
    pub duration_ms: u64,
    /// Tiempo extra, sin pérdida y con los bots quietos, para comprobar que todo converge.
    pub settle_ms: u64,
    pub net: SimConfig,
    pub game: GameConfig,
    pub step_ms: u64,
    pub master: Vec<u8>,
}

impl Default for Scenario {
    fn default() -> Self {
        Scenario {
            clients: 100,
            duration_ms: 20_000,
            settle_ms: 3000,
            net: SimConfig {
                latency_ms: 30,
                jitter_ms: 10,
                loss: 0.02,
                duplicate: 0.0,
                seed: 1,
            },
            game: GameConfig::default(),
            step_ms: 10,
            master: b"loadtest-master-key".to_vec(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub clients: usize,
    pub playing: usize,
    pub entities: usize,
    pub npcs: usize,
    pub sim_seconds: f64,
    pub wall_seconds: f64,

    pub ticks: u64,
    pub tick_avg_us: f64,
    pub tick_max_us: u64,
    pub tick_budget_us: u64,
    /// Fracción de NPC que se simuló de media (el resto estaba apagado por no haber jugadores cerca).
    pub simulated_fraction: f64,
    pub simulated_avg: f64,

    pub snapshots_full: u64,
    pub snapshots_delta: u64,
    pub avg_full_bytes: f64,
    pub avg_delta_bytes: f64,
    pub visible_avg: f64,

    /// Carga útil UDP, medida solo en el tramo estable (sin el arranque).
    pub down_kbps_per_client: f64,
    pub up_kbps_per_client: f64,
    /// Lo mismo contando 28 bytes de cabeceras IPv4+UDP por paquete.
    pub down_kbps_with_headers: f64,
    pub up_kbps_with_headers: f64,

    pub rtt_avg_ms: f64,
    pub correction_avg_units: f64,
    pub correction_max_units: f64,
    pub corrections_per_client_per_s: f64,

    /// Tras el tramo final sin pérdida: jugadores cuya posición predicha coincide exactamente con la del servidor.
    pub final_exact: usize,
    pub final_max_error_units: f64,
    /// Clientes cuyo último snapshot coincide con el último enviado por el servidor / total comparable.
    pub view_matches: usize,
    pub view_compared: usize,

    pub net_sent: u64,
    pub net_dropped: u64,
    pub server_transport: tsofgw_transport::TransportStats,
    pub game: crate::GameStats,
}

type Traffic = (u64, u64, u64, u64);

/// Bytes y paquetes enviados hasta ahora: (servidor, paquetes servidor, clientes, paquetes clientes).
fn traffic(server: &GameServer<SimLink>, bots: &[Bot<SimLink>]) -> Traffic {
    let ts = server.transport_stats();
    let up = bots
        .iter()
        .map(|b| {
            let t = b.client.transport_stats();
            (t.bytes_out, t.packets_out)
        })
        .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    (ts.bytes_out, ts.packets_out, up.0, up.1)
}

pub fn run(s: &Scenario) -> Report {
    let wall = Instant::now();
    let net = SimNet::new(s.net.clone());
    let authority = Authority::new(&s.master);
    let mut server = GameServer::new(
        net.endpoint(),
        authority.clone(),
        s.game.clone(),
        ServerConfig::default(),
        Box::new(DeterministicEntropy(s.net.seed ^ 0xabcd)),
    );
    let server_addr = server.link().addr();

    let mut bots: Vec<Bot<SimLink>> = (0..s.clients)
        .map(|i| {
            let client = GameClient::new(
                net.endpoint(),
                server_addr,
                authority.issue(1000 + i as u64),
                ClientConfig::default(),
                Box::new(DeterministicEntropy(s.net.seed.wrapping_mul(31) + i as u64)),
            );
            Bot::new(client, s.net.seed.wrapping_add(i as u64 * 7919))
        })
        .collect();

    let measure_from = (s.duration_ms / 3).min(5000);
    let moving_until = s.duration_ms;
    let total = s.duration_ms + s.settle_ms;
    let mut window_start: Option<(u64, Traffic)> = None;
    let mut window_end: Option<(u64, Traffic)> = None;
    let mut now = 0u64;
    let mut frozen = false;
    while now < total {
        now += s.step_ms;
        net.set_now(now);
        if !frozen && now >= moving_until {
            frozen = true;
            window_end = Some((now, traffic(&server, &bots)));
            for b in &mut bots {
                b.freeze();
            }
            // El tramo final no tiene pérdida ni duplicados: debe converger todo.
            let mut quiet = s.net.clone();
            quiet.loss = 0.0;
            quiet.duplicate = 0.0;
            net.set_config(quiet);
        }
        server.update(now);
        for b in &mut bots {
            b.update(now);
        }
        if window_start.is_none() && now >= measure_from {
            window_start = Some((now, traffic(&server, &bots)));
        }
    }

    let g = server.stats().clone();
    let ts = server.transport_stats().clone();
    let playing = bots.iter().filter(|b| b.client.is_playing()).count();
    let mut r = Report {
        clients: s.clients,
        playing,
        entities: server.entity_count(),
        npcs: s.game.npc_count,
        sim_seconds: total as f64 / 1000.0,
        ticks: g.ticks,
        tick_budget_us: s.game.tick_ms * 1000,
        tick_max_us: g.tick_time_max_us,
        snapshots_full: g.snapshots_full,
        snapshots_delta: g.snapshots_delta,
        net_sent: net.stats().sent,
        net_dropped: net.stats().dropped,
        server_transport: ts.clone(),
        game: g.clone(),
        ..Default::default()
    };
    if g.ticks > 0 {
        r.tick_avg_us = g.tick_time_us as f64 / g.ticks as f64;
        r.simulated_avg = g.npcs_simulated as f64 / g.ticks as f64;
        if g.npcs_total > 0 {
            r.simulated_fraction = g.npcs_simulated as f64 / g.npcs_total as f64;
        }
    }
    if g.snapshots_full > 0 {
        r.avg_full_bytes = g.bytes_full as f64 / g.snapshots_full as f64;
    }
    if g.snapshots_delta > 0 {
        r.avg_delta_bytes = g.bytes_delta as f64 / g.snapshots_delta as f64;
    }

    // Ancho de banda del tramo estable: desde que pasó el arranque hasta que los bots se detienen.
    if let (Some((t0, a)), Some((t1, b))) = (window_start, window_end) {
        if t1 > t0 {
            let secs = (t1 - t0) as f64 / 1000.0;
            let n = playing.max(1) as f64;
            let kbps = |bytes: u64, packets: u64, headers: bool| {
                let total = bytes as f64 + if headers { 28.0 * packets as f64 } else { 0.0 };
                total * 8.0 / secs / n / 1000.0
            };
            r.down_kbps_per_client = kbps(b.0 - a.0, b.1 - a.1, false);
            r.down_kbps_with_headers = kbps(b.0 - a.0, b.1 - a.1, true);
            r.up_kbps_per_client = kbps(b.2 - a.2, b.3 - a.3, false);
            r.up_kbps_with_headers = kbps(b.2 - a.2, b.3 - a.3, true);
        }
    }

    let mut rtts = Vec::new();
    let (mut corr_n, mut corr_sum, mut corr_max) = (0u64, 0u64, 0u32);
    let mut visible_sum = 0usize;
    let mut visible_n = 0usize;
    for (i, b) in bots.iter().enumerate() {
        if let Some(rtt) = b.client.rtt_ms() {
            rtts.push(rtt);
        }
        let cs = b.client.stats();
        corr_n += cs.corrections;
        corr_sum += cs.correction_sum_q;
        corr_max = corr_max.max(cs.correction_max_q);
        let pid = 1000 + i as u64;
        if let (Some(pred), Some(auth)) = (b.client.predicted(), server.player_position(pid)) {
            let err = (0..3)
                .map(|k| (pred.pos[k] as f64 - auth[k] as f64).powi(2))
                .sum::<f64>()
                .sqrt()
                / 64.0;
            if err == 0.0 {
                r.final_exact += 1;
            }
            r.final_max_error_units = r.final_max_error_units.max(err);
        }
        if let Some(cl) = b.client.latest_snapshot() {
            visible_sum += cl.entities.len();
            visible_n += 1;
            // Se compara con lo que el servidor envió en ese mismo tick (el último enviado aún puede ir por la red).
            if let Some(sv) = server.snapshot_at(pid, cl.tick) {
                r.view_compared += 1;
                if cl.entities == sv.entities {
                    r.view_matches += 1;
                }
            }
        }
    }
    if !rtts.is_empty() {
        r.rtt_avg_ms = rtts.iter().sum::<f64>() / rtts.len() as f64;
    }
    if corr_n > 0 {
        r.correction_avg_units = corr_sum as f64 / corr_n as f64 / 64.0;
    }
    r.correction_max_units = corr_max as f64 / 64.0;
    if playing > 0 {
        r.corrections_per_client_per_s = corr_n as f64 / playing as f64 / (total as f64 / 1000.0);
    }
    if visible_n > 0 {
        r.visible_avg = visible_sum as f64 / visible_n as f64;
    }
    r.wall_seconds = wall.elapsed().as_secs_f64();
    r
}

impl Report {
    pub fn render(&self) -> String {
        let pct = |a: f64| a * 100.0;
        let per_tick = |us: u64| us as f64 / self.ticks.max(1) as f64;
        let delta_share = if self.snapshots_full + self.snapshots_delta > 0 {
            self.snapshots_delta as f64 / (self.snapshots_full + self.snapshots_delta) as f64
        } else {
            0.0
        };
        let ratio = if self.avg_full_bytes > 0.0 && self.avg_delta_bytes > 0.0 {
            self.avg_full_bytes / self.avg_delta_bytes
        } else {
            0.0
        };
        format!(
            "\
== Prueba de carga TSOFGW ==
Clientes: {playing}/{clients} jugando | NPC: {npcs} | entidades en el mundo: {entities}
Tiempo simulado: {sim:.1} s (CPU real: {wall:.1} s)

Servidor (simulación + snapshots, un solo hilo)
  ticks: {ticks}, media {avg:.0} µs, máximo {max} µs, presupuesto {budget} µs ({used:.1} % usado de media)
  NPC simulados de media: {sim_avg:.0} de {npcs} ({sim_pct:.1} %) → el resto está apagado por no haber jugadores cerca

Reparto del tiempo de tick (media por tick)
  inputs {pi:.0} µs | NPC {pn:.0} µs | interés {pint:.0} µs | construir snapshots {pb:.0} µs | codificar {pe:.0} µs | enviar (MAC + red) {ps:.0} µs

Interés y snapshots
  entidades visibles por jugador: {vis:.0} de media
  snapshots: {full} completos + {delta} delta ({dshare:.1} % delta)
  tamaño medio de mensaje: completo {fb:.0} B, delta {db:.0} B (el delta ocupa {ratio:.1}× menos)

Red (tramo estable, por cliente; carga UDP / con cabeceras IP+UDP)
  bajada: {down:.1} kbps / {downh:.1} kbps
  subida: {up:.1} kbps / {uph:.1} kbps
  RTT medio hasta la confirmación: {rtt:.0} ms (incluye la espera al siguiente tick) | paquetes: {sent} enviados, {dropped} perdidos en la red simulada

Predicción de cliente
  correcciones: media {cavg:.2} u, máxima {cmax:.2} u, {cps:.2} por cliente y segundo

Convergencia al final (sin pérdida)
  posición predicha = posición del servidor: {fe}/{playing} exactas (error máximo {femax:.3} u)
  vista del cliente = lo que envió el servidor: {vm}/{vc} comparables idénticas

Defensas del servidor
  paquetes con etiqueta inválida {badtag}, repetidos {replays}, mal formados {malformed}, limitados {rate}
  inputs: {stale} copias redundantes o repetidas ignoradas, {overflow} descartados por cola llena | expulsiones: {kicks}
",
            playing = self.playing,
            clients = self.clients,
            npcs = self.npcs,
            entities = self.entities,
            sim = self.sim_seconds,
            wall = self.wall_seconds,
            pi = per_tick(self.game.phase_inputs_us),
            pn = per_tick(self.game.phase_npc_us),
            pint = per_tick(self.game.phase_interest_us),
            pb = per_tick(self.game.phase_build_us),
            pe = per_tick(self.game.phase_encode_us),
            ps = per_tick(self.game.phase_send_us),
            ticks = self.ticks,
            avg = self.tick_avg_us,
            max = self.tick_max_us,
            budget = self.tick_budget_us,
            used = pct(self.tick_avg_us / self.tick_budget_us.max(1) as f64),
            sim_avg = self.simulated_avg,
            sim_pct = pct(self.simulated_fraction),
            vis = self.visible_avg,
            full = self.snapshots_full,
            delta = self.snapshots_delta,
            dshare = pct(delta_share),
            fb = self.avg_full_bytes,
            db = self.avg_delta_bytes,
            ratio = ratio,
            down = self.down_kbps_per_client,
            downh = self.down_kbps_with_headers,
            up = self.up_kbps_per_client,
            uph = self.up_kbps_with_headers,
            rtt = self.rtt_avg_ms,
            sent = self.net_sent,
            dropped = self.net_dropped,
            cavg = self.correction_avg_units,
            cmax = self.correction_max_units,
            cps = self.corrections_per_client_per_s,
            fe = self.final_exact,
            femax = self.final_max_error_units,
            vm = self.view_matches,
            vc = self.view_compared,
            badtag = self.server_transport.bad_tag,
            replays = self.server_transport.replays,
            malformed = self.game.malformed + self.server_transport.malformed,
            rate = self.server_transport.rate_limited,
            stale = self.game.inputs_stale,
            overflow = self.game.inputs_overflow,
            kicks = self.game.kicks,
        )
    }
}
