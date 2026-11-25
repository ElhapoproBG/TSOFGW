//! Pruebas de extremo a extremo: ejecutan el binario `tsofgw` de verdad.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn tsofgw(dir: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tsofgw"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}
fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}
fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tsofgw-test-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn version_y_ayuda() {
    let d = temp("ver");
    let o = tsofgw(&d, &["version"]);
    assert!(o.status.success());
    assert!(text(&o.stdout).starts_with("tsofgw 0."));
    assert!(text(&tsofgw(&d, &["help"]).stdout).contains("USO:"));
    let o = tsofgw(&d, &["nada"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("comando desconocido"));
}

#[test]
fn hash_de_un_archivo() {
    let d = temp("hash");
    fs::write(d.join("a.txt"), b"abc").unwrap();
    let o = tsofgw(&d, &["hash", "a.txt"]);
    assert!(text(&o.stdout)
        .starts_with("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
}

#[test]
fn empaquetar_firmar_y_verificar() {
    let d = temp("pack");
    fs::write(d.join("mod.bin"), b"contenido del modulo").unwrap();
    fs::write(d.join("clave.txt"), "clave-secreta\n").unwrap();

    let o = tsofgw(
        &d,
        &[
            "pack",
            "mod.bin",
            "--name",
            "fisica",
            "--version",
            "1.0",
            "--caps",
            "net,disk",
            "--signer",
            "oficial",
            "--key-file",
            "clave.txt",
        ],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(d.join("mod.bin.manifest").exists());

    let ok = tsofgw(
        &d,
        &[
            "verify",
            "mod.bin.manifest",
            "mod.bin",
            "--signer",
            "oficial",
            "--key-file",
            "clave.txt",
        ],
    );
    assert!(ok.status.success(), "{}", text(&ok.stderr));
    assert!(text(&ok.stdout).contains("OK: fisica v1.0"));

    // payload alterado
    fs::write(d.join("mod.bin"), b"contenido alterado").unwrap();
    let mal = tsofgw(
        &d,
        &[
            "verify",
            "mod.bin.manifest",
            "mod.bin",
            "--signer",
            "oficial",
            "--key-file",
            "clave.txt",
        ],
    );
    assert!(!mal.status.success());
    assert!(text(&mal.stderr).contains("HashMismatch"));

    // clave incorrecta
    fs::write(d.join("mod.bin"), b"contenido del modulo").unwrap();
    fs::write(d.join("otra.txt"), "otra-clave").unwrap();
    let mal = tsofgw(
        &d,
        &[
            "verify",
            "mod.bin.manifest",
            "mod.bin",
            "--signer",
            "oficial",
            "--key-file",
            "otra.txt",
        ],
    );
    assert!(text(&mal.stderr).contains("BadSignature"));

    // la clave no se acepta en la línea de comandos
    let o = tsofgw(
        &d,
        &[
            "pack",
            "mod.bin",
            "--name",
            "x",
            "--version",
            "1.0",
            "--signer",
            "oficial",
        ],
    );
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("--key-file"));
}

#[test]
fn sin_firma_requiere_permiso_explicito() {
    let d = temp("nosign");
    fs::write(d.join("m.bin"), b"x").unwrap();
    assert!(tsofgw(
        &d,
        &[
            "pack",
            "m.bin",
            "--name",
            "m",
            "--version",
            "0.1",
            "--caps",
            "net"
        ]
    )
    .status
    .success());
    let o = tsofgw(&d, &["verify", "m.bin.manifest", "m.bin"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("UnsignedNotAllowed"));
    let o = tsofgw(
        &d,
        &["verify", "m.bin.manifest", "m.bin", "--allow-unsigned"],
    );
    assert!(o.status.success());
    assert!(text(&o.stdout).contains("AVISO"));
}

#[test]
fn demo_y_dot() {
    let d = temp("demo");
    let o = tsofgw(
        &d,
        &[
            "demo",
            "--entities",
            "3000",
            "--players",
            "10",
            "--ticks",
            "10",
        ],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let t = text(&o.stdout);
    assert!(t.contains("Simulación selectiva"));
    assert!(t.contains("Descartados por presupuesto: analytics"));

    let pc = text(
        &tsofgw(
            &d,
            &[
                "demo",
                "--profile",
                "pc",
                "--entities",
                "3000",
                "--players",
                "10",
                "--ticks",
                "5",
            ],
        )
        .stdout,
    );
    assert!(pc.contains("Descartados por presupuesto: ninguno"));

    assert!(
        !tsofgw(&d, &["demo", "--players", "50", "--entities", "10"])
            .status
            .success()
    );
    assert!(!tsofgw(&d, &["demo", "--profile", "zzz"]).status.success());

    let dot = text(&tsofgw(&d, &["dot"]).stdout);
    assert!(dot.starts_with("digraph tsofgw {"));
    assert!(dot.contains("mmo_server"));
}

#[test]
fn nuevo_proyecto() {
    let d = temp("new");
    let o = tsofgw(&d, &["new", "mi-juego"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(d.join("mi-juego/Cargo.toml").exists());
    assert!(d.join("mi-juego/src/main.rs").exists());
    assert!(text(&fs::read(d.join("mi-juego/Cargo.toml")).unwrap()).contains("tsofgw-core"));
    // no pisa lo existente ni acepta nombres raros
    assert!(!tsofgw(&d, &["new", "mi-juego"]).status.success());
    assert!(!tsofgw(&d, &["new", "../fuera"]).status.success());
}

#[test]
fn render_escribe_un_png_valido_con_objetos() {
    let d = temp("render");
    let o = tsofgw(
        &d,
        &[
            "render",
            "--scene",
            "triangulo",
            "--size",
            "96x64",
            "-o",
            "t.png",
        ],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let bytes = fs::read(d.join("t.png")).unwrap();
    assert_eq!(
        &bytes[..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
    );
    let (w, h, px) = tsofgw_soft::png::decode_rgba(&bytes).unwrap();
    assert_eq!((w, h), (96, 64));
    let fondo = [13u8, 15, 23];
    let (pixeles, _) = px.as_chunks::<4>();
    let dibujados = pixeles.iter().filter(|p| p[..3] != fondo).count();
    assert!(dibujados > 200, "solo {dibujados} píxeles con objetos");
    assert!(text(&o.stdout).contains("96x64"));

    // estéreo: el ancho es el doble del de un ojo
    let o = tsofgw(
        &d,
        &["render", "--stereo", "--size", "100x50", "-o", "e.png"],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let (w, h, _) = tsofgw_soft::png::decode_rgba(&fs::read(d.join("e.png")).unwrap()).unwrap();
    assert_eq!((w, h), (100, 50));

    // errores claros
    assert!(!tsofgw(&d, &["render", "--scene", "nada"]).status.success());
    assert!(!tsofgw(&d, &["render", "--size", "2x2"]).status.success());
}

#[test]
fn opciones_desconocidas_se_rechazan() {
    let d = temp("flags");
    let o = tsofgw(&d, &["render", "--out", "x.png"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("opción desconocida: --out"));
    assert!(!d.join("x.png").exists());
    let o = tsofgw(&d, &["loadtest", "--clientes", "5"]);
    assert!(!o.status.success());
    let o = tsofgw(&d, &["demo", "--profile"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("falta el valor"));
}

#[test]
fn keygen_token_y_no_sobrescribe() {
    let d = temp("keygen");
    let o = tsofgw(&d, &["keygen", "-o", "master.key"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let key = fs::read_to_string(d.join("master.key")).unwrap();
    assert_eq!(key.trim().len(), 64);
    assert!(key.trim().chars().all(|c| c.is_ascii_hexdigit()));

    // No pisa una clave existente.
    let o = tsofgw(&d, &["keygen", "-o", "master.key"]);
    assert!(!o.status.success());
    assert_eq!(fs::read_to_string(d.join("master.key")).unwrap(), key);

    // La clave de una cuenta es estable y distinta de la de otra.
    let t1 = text(
        &tsofgw(
            &d,
            &["token", "--master-file", "master.key", "--player", "7"],
        )
        .stdout,
    );
    let t1b = text(
        &tsofgw(
            &d,
            &["token", "--master-file", "master.key", "--player", "7"],
        )
        .stdout,
    );
    let t2 = text(
        &tsofgw(
            &d,
            &["token", "--master-file", "master.key", "--player", "8"],
        )
        .stdout,
    );
    assert_eq!(t1, t1b);
    assert_ne!(t1, t2);
    assert!(t1.contains("cuenta: 7"));
    let o = tsofgw(&d, &["token", "--master-file", "master.key"]);
    assert!(!o.status.success());
}

#[test]
fn loadtest_simulado_produce_un_informe_coherente() {
    let d = temp("load");
    let o = tsofgw(
        &d,
        &[
            "loadtest",
            "--clients",
            "20",
            "--npcs",
            "1000",
            "--seconds",
            "6",
            "--loss",
            "0.03",
        ],
    );
    let out = text(&o.stdout);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(out.contains("20/20 jugando"), "{out}");
    assert!(out.contains("20/20 exactas"), "{out}");
    assert!(out.contains("comparables idénticas"));
    assert!(out.contains("Reparto del tiempo de tick"));
    // Los parámetros absurdos se rechazan.
    assert!(!tsofgw(&d, &["loadtest", "--loss", "2"]).status.success());
    assert!(!tsofgw(&d, &["loadtest", "--seconds", "1"]).status.success());
}

#[test]
fn servidor_y_bots_reales_por_udp() {
    let d = temp("udp");
    assert!(tsofgw(&d, &["keygen", "-o", "m.key"]).status.success());
    // Un puerto libre (se abre y se cierra para saber cuál está disponible).
    let port = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    let bind = format!("127.0.0.1:{port}");
    let server = Command::new(env!("CARGO_BIN_EXE_tsofgw"))
        .current_dir(&d)
        .args([
            "serve",
            "--master-file",
            "m.key",
            "--bind",
            &bind,
            "--npcs",
            "500",
            "--duration",
            "9",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let bots = tsofgw(
        &d,
        &[
            "bots",
            "--master-file",
            "m.key",
            "--connect",
            &bind,
            "--count",
            "5",
            "--duration",
            "6",
        ],
    );
    let bots_out = text(&bots.stdout);
    assert!(bots.status.success(), "{}", text(&bots.stderr));
    assert!(bots_out.contains("5 bots conectando"), "{bots_out}");
    let out = server.wait_with_output().unwrap();
    let srv = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(srv.contains("escuchando en"), "{srv}");
    // A los 5 s ya debe haber informado de los 5 jugadores conectados.
    assert!(
        srv.contains("jugadores 5"),
        "el servidor no vio a los 5 bots:\n{srv}"
    );
    assert!(!srv.contains("etiquetas inválidas 1"));
}

#[test]
fn bots_con_otra_clave_no_entran() {
    let d = temp("udp2");
    assert!(tsofgw(&d, &["keygen", "-o", "a.key"]).status.success());
    assert!(tsofgw(&d, &["keygen", "-o", "b.key"]).status.success());
    let port = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    let bind = format!("127.0.0.1:{port}");
    let server = Command::new(env!("CARGO_BIN_EXE_tsofgw"))
        .current_dir(&d)
        .args([
            "serve",
            "--master-file",
            "a.key",
            "--bind",
            &bind,
            "--npcs",
            "100",
            "--duration",
            "8",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let _ = tsofgw(
        &d,
        &[
            "bots",
            "--master-file",
            "b.key",
            "--connect",
            &bind,
            "--count",
            "3",
            "--duration",
            "6",
        ],
    );
    let out = server.wait_with_output().unwrap();
    let srv = text(&out.stdout);
    assert!(srv.contains("jugadores 0"), "{srv}");
    assert!(!srv.contains("jugadores 3"));
}

#[test]
fn loadtest_de_cluster_simulado_y_sobre_udp_real() {
    let d = temp("cluster-lt");
    // Red simulada: determinista y rápida.
    let o = tsofgw(
        &d,
        &[
            "loadtest",
            "--shards",
            "2x1",
            "--clients",
            "12",
            "--patrol",
            "8",
            "--npcs",
            "800",
            "--seconds",
            "10",
        ],
    );
    let t = text(&o.stdout);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(t.contains("Cluster TSOFGW: 2×1 = 2 shards"), "{t}");
    assert!(t.contains("NPC: 0 perdidos, 0 duplicados de 800"), "{t}");
    assert!(
        t.contains("jugadores: 0 perdidos, 0 duplicados de 12"),
        "{t}"
    );
    assert!(
        t.contains("entidades que faltaban: 0 | entidades inexistentes: 0"),
        "{t}"
    );
    assert!(
        !t.contains("traspasos de jugador: 0 iniciados"),
        "debe haber cruzado el borde:\n{t}"
    );

    // Sockets UDP reales de este equipo, con el bus entre shards autenticado.
    let o = tsofgw(
        &d,
        &[
            "loadtest",
            "--shards",
            "2x1",
            "--udp",
            "--clients",
            "12",
            "--patrol",
            "8",
            "--npcs",
            "800",
            "--seconds",
            "12",
        ],
    );
    let t = text(&o.stdout);
    assert!(o.status.success(), "{}{}", t, text(&o.stderr));
    assert!(t.contains("UDP real"), "{t}");
    assert!(t.contains("datagramas rechazados: 0"), "{t}");
    assert!(t.contains("NPC: 0 perdidos, 0 duplicados de 800"), "{t}");
    assert!(
        t.contains("jugadores: 0 perdidos, 0 duplicados de 12"),
        "{t}"
    );
    assert!(
        t.contains("entidades que faltaban: 0 | entidades inexistentes: 0"),
        "{t}"
    );
    assert!(
        !t.contains("traspasos de jugador: 0 iniciados"),
        "debe haber cruzado el borde:\n{t}"
    );
}

#[test]
fn opciones_de_cluster_incoherentes_se_rechazan() {
    let d = temp("cluster-err");
    assert!(tsofgw(&d, &["keygen", "-o", "m.key"]).status.success());
    assert!(tsofgw(&d, &["keygen", "-o", "c.key"]).status.success());
    let fails = |args: &[&str], needle: &str| {
        let o = tsofgw(&d, args);
        assert!(!o.status.success(), "{args:?} debía fallar");
        let e = text(&o.stderr);
        assert!(e.contains(needle), "{args:?}: {e}");
    };
    // Opciones de cluster sin --shard.
    fails(
        &["serve", "--master-file", "m.key", "--grid", "2x2"],
        "solo tiene sentido con --shard",
    );
    // Faltan datos del cluster.
    fails(
        &["serve", "--master-file", "m.key", "--shard", "0"],
        "falta --grid",
    );
    fails(
        &[
            "serve",
            "--master-file",
            "m.key",
            "--shard",
            "0",
            "--grid",
            "2x1",
        ],
        "falta --peers",
    );
    fails(
        &[
            "serve",
            "--master-file",
            "m.key",
            "--shard",
            "0",
            "--grid",
            "2x1",
            "--peers",
            "127.0.0.1:7001,127.0.0.1:7002",
        ],
        "falta --cluster-key-file",
    );
    // Direcciones que no cuadran con la rejilla, shard inexistente, rejilla imposible.
    let base = [
        "serve",
        "--master-file",
        "m.key",
        "--cluster-key-file",
        "c.key",
        "--bind",
        "127.0.0.1:0",
    ];
    let with = |extra: &[&'static str]| -> Vec<&'static str> {
        let mut v: Vec<&str> = base.to_vec();
        v.extend_from_slice(extra);
        v
    };
    fails(
        &with(&[
            "--shard",
            "0",
            "--grid",
            "2x2",
            "--peers",
            "127.0.0.1:7001,127.0.0.1:7002",
        ]),
        "debe tener 4 direcciones",
    );
    fails(
        &with(&[
            "--shard",
            "5",
            "--grid",
            "2x1",
            "--peers",
            "127.0.0.1:7001,127.0.0.1:7002",
        ]),
        "la rejilla 2x1 tiene 2 shards",
    );
    fails(
        &with(&["--shard", "0", "--grid", "0x2", "--peers", "127.0.0.1:7001"]),
        "entre 1 y 256",
    );
    fails(
        &with(&["--shard", "0", "--grid", "dos", "--peers", "127.0.0.1:7001"]),
        "COLUMNASxFILAS",
    );
    // Una clave de cluster demasiado corta.
    std::fs::write(d.join("short.key"), "corta\n").unwrap();
    fails(
        &[
            "serve",
            "--master-file",
            "m.key",
            "--cluster-key-file",
            "short.key",
            "--shard",
            "0",
            "--grid",
            "1x1",
            "--peers",
            "127.0.0.1:7001",
        ],
        "al menos 16 bytes",
    );
    // loadtest.
    fails(&["loadtest", "--shards", "0x3"], "entre 1 y 256");
    fails(&["loadtest", "--shards", "x"], "COLUMNASxFILAS");
    fails(
        &["loadtest", "--shards", "2x2", "--bus-loss", "2"],
        "--bus-loss",
    );
}

#[test]
fn un_cluster_de_procesos_reales_traspasa_jugadores_entre_shards() {
    let d = temp("cluster-proc");
    assert!(tsofgw(&d, &["keygen", "-o", "m.key"]).status.success());
    assert!(tsofgw(&d, &["keygen", "-o", "c.key"]).status.success());
    // Cuatro puertos libres: clientes y bus de cada uno de los dos shards.
    let ports: Vec<u16> = {
        let socks: Vec<_> = (0..4)
            .map(|_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
            .collect();
        socks
            .iter()
            .map(|s| s.local_addr().unwrap().port())
            .collect()
    };
    let (c0, c1, b0, b1) = (ports[0], ports[1], ports[2], ports[3]);
    let peers = format!("127.0.0.1:{b0},127.0.0.1:{b1}");
    let spawn = |shard: u16, bind: u16| {
        Command::new(env!("CARGO_BIN_EXE_tsofgw"))
            .current_dir(&d)
            .args([
                "serve",
                "--master-file",
                "m.key",
                "--cluster-key-file",
                "c.key",
                "--shard",
                &shard.to_string(),
                "--grid",
                "2x1",
                "--peers",
                &peers,
                "--bind",
                &format!("127.0.0.1:{bind}"),
                "--npcs",
                "1000",
                "--spawn-half",
                "40",
                "--duration",
                "17",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let s0 = spawn(0, c0);
    // El segundo shard arranca un poco más tarde: sus ticks no coinciden con los del primero.
    std::thread::sleep(std::time::Duration::from_millis(700));
    let s1 = spawn(1, c1);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let shards = format!("127.0.0.1:{c0},127.0.0.1:{c1}");
    let bots = tsofgw(
        &d,
        &[
            "bots",
            "--master-file",
            "m.key",
            "--connect",
            &format!("127.0.0.1:{c0}"),
            "--shards",
            &shards,
            "--count",
            "10",
            "--patrol",
            "8",
            "--patrol-ms",
            "4000",
            "--duration",
            "11",
        ],
    );
    let bots_out = text(&bots.stdout);
    assert!(bots.status.success(), "{}", text(&bots.stderr));
    let (o0, o1) = (
        s0.wait_with_output().unwrap(),
        s1.wait_with_output().unwrap(),
    );
    let (t0, t1) = (text(&o0.stdout), text(&o1.stdout));
    assert!(
        o0.status.success() && o1.status.success(),
        "{}{}",
        text(&o0.stderr),
        text(&o1.stderr)
    );
    assert!(
        t0.contains("shard 0 de 2") && t1.contains("shard 1 de 2"),
        "{t0}\n{t1}"
    );
    // Los bots cruzaron de un shard a otro sin un solo fallo.
    assert!(bots_out.contains("jugando 10/10"), "{bots_out}");
    assert!(bots_out.contains("(0 fallidos)"), "{bots_out}");
    assert!(!bots_out.contains("cambios de shard 0 "), "{bots_out}");
    // Los dos shards intercambiaron jugadores y el bus no rechazó nada legítimo.
    let last = |t: &str| -> String {
        t.lines()
            .rev()
            .find(|l| l.contains("cluster:"))
            .expect(t)
            .to_string()
    };
    let (l0, l1) = (last(&t0), last(&t1));
    for l in [&l0, &l1] {
        assert!(l.contains("bus: rechazados 0, mal formados 0"), "{l}");
        assert!(
            !l.contains("(cancelados 1") && !l.contains("(cancelados 2"),
            "{l}"
        );
    }
    assert!(
        !l0.contains("traspasos de jugador 0/0"),
        "el shard 0 no entregó a nadie:\n{t0}"
    );
    assert!(
        !l1.contains("llegadas 0)"),
        "el shard 1 no recibió a nadie:\n{t1}"
    );
}

// ------------------------------------------------------------------------------------------------
// Persistencia, directorio de sesiones y herramientas del almacén (0.5.0)
// ------------------------------------------------------------------------------------------------

use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Un proceso `tsofgw` en marcha cuyo texto (salida y errores) va a un archivo: nada que se llene.
/// Si la prueba falla con procesos vivos, al soltarlo se mata para no dejarlo huérfano.
struct Proc {
    child: Child,
    log: PathBuf,
}

impl Proc {
    fn spawn(dir: &PathBuf, name: &str, args: &[&str]) -> Proc {
        let log = dir.join(format!("{name}.log"));
        let out = fs::File::create(&log).unwrap();
        let err = out.try_clone().unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_tsofgw"))
            .current_dir(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .unwrap();
        Proc { child, log }
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Espera a que el texto aparezca en el registro.
    fn wait_for(&self, needle: &str, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if self.log().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Espera a que termine por sí mismo; si se pasa del plazo, lo mata y falla la prueba.
    fn finish(&mut self, secs: u64) -> (ExitStatus, String) {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return (st, self.log());
            }
            if Instant::now() > end {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("el proceso no terminó en {secs} s:\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Un corte de luz: muerte inmediata, sin cierre limpio ni nada que guardar.
    fn kill9(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_udp_ports(n: usize) -> Vec<u16> {
    let socks: Vec<_> = (0..n)
        .map(|_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
        .collect();
    socks
        .iter()
        .map(|s| s.local_addr().unwrap().port())
        .collect()
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// El número que sigue a `label` en `t` (`"cuentas residentes: 7"` → 7).
fn number_after(t: &str, label: &str) -> u64 {
    let i = t
        .find(label)
        .unwrap_or_else(|| panic!("no aparece «{label}» en:\n{t}"))
        + label.len();
    t[i..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("«{label}» no va seguido de un número en:\n{t}"))
}

#[test]
fn un_servidor_con_datos_se_restaura_y_rechaza_claves_y_mundos_ajenos() {
    let d = temp("persist-solo");
    assert!(tsofgw(&d, &["keygen", "-o", "m.key"]).status.success());
    assert!(tsofgw(&d, &["keygen", "-o", "otra.key"]).status.success());
    let serve = |key: &str, npcs: &str| {
        tsofgw(
            &d,
            &[
                "serve",
                "--master-file",
                key,
                "--bind",
                "127.0.0.1:0",
                "--npcs",
                npcs,
                "--duration",
                "2",
                "--data-dir",
                "datos",
            ],
        )
    };

    // Primer arranque: mundo nuevo. Termina limpio.
    let a = serve("m.key", "400");
    let out = text(&a.stdout);
    assert!(a.status.success(), "{}", text(&a.stderr));
    assert!(out.contains("primer arranque"), "{out}");
    assert!(out.contains("Cierre limpio"), "{out}");

    // Segundo arranque: el mundo vuelve tal cual estaba.
    let b = serve("m.key", "400");
    let out = text(&b.stdout);
    assert!(b.status.success(), "{}", text(&b.stderr));
    assert!(out.contains("restaurado — 400 NPC"), "{out}");
    assert!(!out.contains("primer arranque"), "{out}");

    // Con otra clave no se abre ni se toca: lo guardado está autenticado.
    let c = serve("otra.key", "400");
    assert!(!c.status.success());
    let e = text(&c.stderr);
    assert!(e.contains("no se autentica con esta clave"), "{e}");

    // Con otro mundo (otro número de NPC) tampoco: no se mezclan mundos.
    let f = serve("m.key", "401");
    assert!(!f.status.success());
    let e = text(&f.stderr);
    assert!(e.contains("pertenece a otro mundo"), "{e}");

    // Los dos rechazos no dañaron nada: la carpeta sigue diciendo lo mismo.
    let info = tsofgw(
        &d,
        &[
            "store",
            "info",
            "--data-dir",
            "datos",
            "--master-file",
            "m.key",
            "--standalone",
        ],
    );
    let t = text(&info.stdout);
    assert!(info.status.success(), "{}", text(&info.stderr));
    assert_eq!(number_after(&t, "NPC:"), 400, "{t}");
    assert!(t.contains("mundo inicializado: sí"), "{t}");
    // Y su dueño no es un shard: con la clave de cluster y un número de shard, otro cantar.
    assert!(tsofgw(&d, &["keygen", "-o", "c.key"]).status.success());
    let mal = tsofgw(
        &d,
        &[
            "store",
            "info",
            "--data-dir",
            "datos",
            "--cluster-key-file",
            "c.key",
            "--shard",
            "0",
        ],
    );
    assert!(!mal.status.success());
    assert!(
        text(&mal.stderr).contains("no se autentica"),
        "{}",
        text(&mal.stderr)
    );
}

/// SIGTERM (lo que manda `docker stop`, `systemctl stop` o un orquestador) debe cerrar el servidor
/// igual que Ctrl-C: guardar, compactar y avisar a los clientes. Solo en unix: en Windows el mismo
/// camino lo recorre Ctrl-C/Ctrl-Break, que una prueba no puede enviar a un proceso hijo.
#[cfg(unix)]
#[test]
fn sigterm_cierra_el_servidor_guardando_su_estado() {
    let d = temp("sigterm");
    assert!(tsofgw(&d, &["keygen", "-o", "m.key"]).status.success());
    let mut p = Proc::spawn(
        &d,
        "srv",
        &[
            "serve",
            "--master-file",
            "m.key",
            "--bind",
            "127.0.0.1:0",
            "--npcs",
            "300",
            "--data-dir",
            "datos",
        ],
    );
    assert!(p.wait_for("Persistencia en datos", 20), "{}", p.log());
    // El manejador se instala justo después de escribir esa línea.
    sleep_ms(1500);
    let pid = p.child.id().to_string();
    let k = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(k.success());
    let (st, log) = p.finish(20);
    assert!(st.success(), "{log}");
    assert!(log.contains("Parada pedida"), "{log}");
    assert!(log.contains("Cierre limpio"), "{log}");
    // El estado quedó compactado: nada en el diario, todo en el checkpoint.
    let info = tsofgw(
        &d,
        &[
            "store",
            "info",
            "--data-dir",
            "datos",
            "--master-file",
            "m.key",
            "--standalone",
        ],
    );
    let t = text(&info.stdout);
    assert!(info.status.success(), "{}", text(&info.stderr));
    assert_eq!(number_after(&t, "NPC:"), 300, "{t}");
    assert!(
        t.contains("diario desde el último checkpoint: 0.0 KB"),
        "{t}"
    );
}

#[test]
fn opciones_de_persistencia_incoherentes_se_rechazan() {
    let d = temp("persist-err");
    assert!(tsofgw(&d, &["keygen", "-o", "m.key"]).status.success());
    assert!(tsofgw(&d, &["keygen", "-o", "c.key"]).status.success());
    let fails = |args: &[&str], needle: &str| {
        let o = tsofgw(&d, args);
        assert!(!o.status.success(), "{args:?} debía fallar");
        let e = text(&o.stderr);
        assert!(e.contains(needle), "{args:?}: {e}");
    };
    // Opciones de persistencia sin carpeta de datos.
    fails(
        &["serve", "--master-file", "m.key", "--repair"],
        "--repair solo tiene sentido con --data-dir",
    );
    fails(
        &["serve", "--master-file", "m.key", "--sync-ticks", "5"],
        "--sync-ticks solo tiene sentido con --data-dir",
    );
    fails(
        &[
            "serve",
            "--master-file",
            "m.key",
            "--directory",
            "127.0.0.1:9",
        ],
        "--directory solo tiene sentido con --data-dir",
    );
    // Valores fuera de rango.
    let with = |extra: &[&'static str]| -> Vec<&'static str> {
        let mut v = vec!["serve", "--master-file", "m.key", "--data-dir", "x"];
        v.extend_from_slice(extra);
        v
    };
    fails(&with(&["--sync-ticks", "0"]), "al menos 1");
    fails(&with(&["--npc-save-ticks", "0"]), "al menos 1");
    fails(&with(&["--compact-mb", "0"]), "entre 1 y 4096");
    fails(&with(&["--compact-mb", "5000"]), "entre 1 y 4096");
    // El directorio es cosa de los shards de un cluster.
    fails(
        &with(&["--directory", "127.0.0.1:9"]),
        "--directory solo tiene sentido con --shard",
    );
    // El directorio exige todo lo del cluster y una carpeta donde guardar.
    fails(&["directory"], "falta --grid");
    fails(
        &[
            "directory",
            "--grid",
            "2x1",
            "--peers",
            "127.0.0.1:7001,127.0.0.1:7002",
        ],
        "falta --cluster-key-file",
    );
    fails(
        &[
            "directory",
            "--grid",
            "2x1",
            "--peers",
            "127.0.0.1:7001",
            "--cluster-key-file",
            "c.key",
            "--data-dir",
            "dd",
            "--bind",
            "127.0.0.1:0",
        ],
        "debe tener 2 direcciones",
    );
    fails(
        &[
            "directory",
            "--grid",
            "2x1",
            "--peers",
            "127.0.0.1:7001,127.0.0.1:7002",
            "--cluster-key-file",
            "c.key",
            "--bind",
            "127.0.0.1:0",
        ],
        "falta --data-dir",
    );
    // Herramientas del almacén.
    fails(&["store"], "falta el subcomando");
    fails(&["store", "nada"], "subcomando desconocido");
    fails(
        &["store", "info", "--data-dir", "x"],
        "indica de quién es la carpeta",
    );
    fails(
        &[
            "store",
            "info",
            "--data-dir",
            "x",
            "--standalone",
            "--directory",
        ],
        "indica de quién es la carpeta",
    );
    fails(
        &[
            "store",
            "rebuild-directory",
            "--shard-dirs",
            "a,b",
            "--data-dir",
            "a",
            "--cluster-key-file",
            "c.key",
        ],
        "no puede ser la carpeta de ningún shard",
    );
    // Una herramienta de consulta no debe dejar un almacén vacío donde el operador se equivocó de ruta.
    fails(
        &[
            "store",
            "info",
            "--data-dir",
            "no-existe",
            "--master-file",
            "m.key",
            "--standalone",
        ],
        "no contiene ningún almacén",
    );
    assert!(!d.join("no-existe").exists());
    fs::create_dir_all(d.join("vacia")).unwrap();
    fails(
        &[
            "store",
            "info",
            "--data-dir",
            "vacia",
            "--master-file",
            "m.key",
            "--standalone",
        ],
        "no contiene ningún almacén",
    );
    assert_eq!(fs::read_dir(d.join("vacia")).unwrap().count(), 0);
    // loadtest.
    fails(
        &["loadtest", "--seconds", "2", "--chaos", "3"],
        "--chaos solo tiene sentido con --shards",
    );
    fails(
        &[
            "loadtest",
            "--shards",
            "2x1",
            "--seconds",
            "2",
            "--chaos-seed",
            "3",
        ],
        "--chaos-seed solo tiene sentido con --chaos",
    );
    fails(
        &[
            "loadtest",
            "--shards",
            "2x1",
            "--seconds",
            "2",
            "--chaos",
            "900",
        ],
        "entre 0 y 500",
    );
    fails(
        &[
            "loadtest",
            "--shards",
            "2x1",
            "--seconds",
            "2",
            "--persist",
            "--udp",
        ],
        "no se combinan con --udp",
    );
}

#[test]
fn loadtest_de_cluster_con_directorio_y_tormenta_de_caidas() {
    let d = temp("chaos-cli");
    let o = tsofgw(
        &d,
        &[
            "loadtest",
            "--shards",
            "2x2",
            "--clients",
            "16",
            "--seconds",
            "8",
            "--npcs",
            "600",
            "--directory",
            "--chaos",
            "4",
            "--chaos-seed",
            "7",
        ],
    );
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out}\n{}", text(&o.stderr));
    assert!(out.contains("Persistencia y sesiones"), "{out}");
    assert!(out.contains("tormenta de caídas: 4 rondas"), "{out}");
    assert!(
        out.contains("jugadores: 0 perdidos, 0 duplicados de 16"),
        "{out}"
    );
    assert!(
        out.contains("duplicadas 0 | perdidas 0 | distintas del directorio 0"),
        "{out}"
    );
    assert!(out.contains("cuentas residentes en dos shards 0"), "{out}");
}

#[test]
fn un_cluster_con_directorio_sobrevive_a_un_kill_9_de_un_shard_y_del_directorio() {
    let d = temp("cluster-kill9");
    assert!(tsofgw(&d, &["keygen", "-o", "m.key"]).status.success());
    assert!(tsofgw(&d, &["keygen", "-o", "c.key"]).status.success());
    // Puertos libres: clientes y bus de los dos shards, y el directorio.
    let p = free_udp_ports(5);
    let (c0, c1, b0, b1, bd) = (p[0], p[1], p[2], p[3], p[4]);
    let peers = format!("127.0.0.1:{b0},127.0.0.1:{b1}");
    let dir_addr = format!("127.0.0.1:{bd}");
    let shard = |name: &str, i: u16, bind: u16, secs: u64| {
        Proc::spawn(
            &d,
            name,
            &[
                "serve",
                "--master-file",
                "m.key",
                "--cluster-key-file",
                "c.key",
                "--shard",
                &i.to_string(),
                "--grid",
                "2x1",
                "--peers",
                &peers,
                "--bind",
                &format!("127.0.0.1:{bind}"),
                "--npcs",
                "1000",
                // Todos aparecen a menos de 10 unidades del borde: los patrulleros lo cruzan sin falta.
                "--spawn-half",
                "10",
                "--data-dir",
                &format!("datos{i}"),
                "--directory",
                &dir_addr,
                "--duration",
                &secs.to_string(),
            ],
        )
    };
    let directory = |name: &str, secs: u64| {
        Proc::spawn(
            &d,
            name,
            &[
                "directory",
                "--grid",
                "2x1",
                "--peers",
                &peers,
                "--cluster-key-file",
                "c.key",
                "--bind",
                &dir_addr,
                "--data-dir",
                "datos-dir",
                "--npcs",
                "1000",
                "--duration",
                &secs.to_string(),
            ],
        )
    };
    let shards = format!("127.0.0.1:{c0},127.0.0.1:{c1}");
    let bots = |secs: u64| {
        tsofgw(
            &d,
            &[
                "bots",
                "--master-file",
                "m.key",
                "--connect",
                &format!("127.0.0.1:{c0}"),
                "--shards",
                &shards,
                "--count",
                "10",
                "--patrol",
                "8",
                "--patrol-ms",
                "4000",
                "--duration",
                &secs.to_string(),
            ],
        )
    };
    let info = |args: &[&str]| -> String {
        let mut a = vec!["store", "info", "--data-dir"];
        a.extend_from_slice(args);
        let o = tsofgw(&d, &a);
        assert!(o.status.success(), "{a:?}: {}", text(&o.stderr));
        text(&o.stdout)
    };
    // «cuentas: 10 (por shard [4, 6])» → "[4, 6]"
    let reparto = |t: &str| -> String {
        let i = t.find("por shard ").expect(t) + "por shard ".len();
        t[i..].split(')').next().unwrap().to_string()
    };

    // Fase 1: directorio y dos shards con disco; diez jugadores patrullan cruzando el borde.
    let mut dir1 = directory("dir1", 60);
    sleep_ms(400);
    let mut s0 = shard("s0", 0, c0, 37);
    sleep_ms(300);
    let mut s1a = shard("s1a", 1, c1, 60);
    sleep_ms(1000);
    let o1 = bots(8);
    let out1 = text(&o1.stdout);
    assert!(o1.status.success(), "{out1}\n{}", text(&o1.stderr));
    assert!(out1.contains("jugando 10/10"), "{out1}");
    assert!(out1.contains("(0 fallidos)"), "{out1}");
    let cambios = |out: &str| {
        let l = out
            .lines()
            .rev()
            .find(|l| l.contains("cambios de shard"))
            .unwrap_or_else(|| panic!("sin informe de bots:\n{out}"));
        number_after(l, "cambios de shard")
    };
    assert!(cambios(&out1) >= 2, "nadie cruzó el borde:\n{out1}");

    // Un corte de luz para el shard 1 y para el directorio: sin cierre limpio, sin guardar nada más.
    s1a.kill9();
    dir1.kill9();
    // Lo que quedó en disco se puede leer (con los procesos muertos, su bloqueo ya no está).
    let tras = info(&["datos-dir", "--directory", "--cluster-key-file", "c.key"]);
    assert_eq!(number_after(&tras, "cuentas:"), 10, "{tras}");
    let tras1 = info(&["datos1", "--shard", "1", "--cluster-key-file", "c.key"]);
    assert!(tras1.contains("mundo inicializado: sí"), "{tras1}");
    assert!(number_after(&tras1, "NPC:") > 0, "{tras1}");

    // Fase 2: los dos vuelven a arrancar con sus carpetas; los mismos jugadores vuelven a entrar.
    let mut dir2 = directory("dir2", 26);
    sleep_ms(300);
    let mut s1b = shard("s1b", 1, c1, 25);
    sleep_ms(1500);
    let o2 = bots(12);
    let out2 = text(&o2.stdout);
    assert!(o2.status.success(), "{out2}\n{}", text(&o2.stderr));
    assert!(out2.contains("jugando 10/10"), "{out2}");
    assert!(out2.contains("(0 fallidos)"), "{out2}");
    assert!(
        cambios(&out2) >= 2,
        "tras el reinicio casi nadie cruzó un borde:\n{out2}"
    );
    // Los jugadores entran por el shard 0 y es el directorio restaurado quien los manda al que tiene su cuenta.
    let entradas = |out: &str| {
        let l = out
            .lines()
            .rev()
            .find(|l| l.contains("entradas redirigidas"))
            .unwrap();
        number_after(l, "entradas redirigidas")
    };
    assert!(
        entradas(&out2) >= 1,
        "nadie fue redirigido al shard de su cuenta tras el reinicio:\n{out2}"
    );

    // Fin: todo se cierra limpio y lo que se restauró era lo que había en disco.
    let (st0, t0) = s0.finish(60);
    let (std, td) = dir2.finish(60);
    let (st1, t1) = s1b.finish(60);
    for (st, t) in [(&st0, &t0), (&std, &td), (&st1, &t1)] {
        assert!(st.success(), "{t}");
        assert!(t.contains("Cierre limpio"), "{t}");
    }
    assert!(
        t1.contains("restaurado"),
        "el shard 1 no restauró su carpeta:\n{t1}"
    );
    assert!(
        td.contains("restaurado — 10 cuentas"),
        "el directorio no restauró sus cuentas:\n{td}"
    );
    assert!(t0.contains("primer arranque"), "{t0}");
    for t in [&t0, &t1, &td] {
        assert!(!t.contains("ERROR"), "{t}");
        assert!(!t.contains("ADVERTENCIA: --repair"), "{t}");
    }

    // Cada cuenta está en exactamente un shard y el directorio dice lo mismo que los shards.
    let i0 = info(&["datos0", "--shard", "0", "--cluster-key-file", "c.key"]);
    let i1 = info(&["datos1", "--shard", "1", "--cluster-key-file", "c.key"]);
    let id = info(&["datos-dir", "--directory", "--cluster-key-file", "c.key"]);
    let (r0, r1) = (
        number_after(&i0, "cuentas residentes:"),
        number_after(&i1, "cuentas residentes:"),
    );
    assert_eq!(r0 + r1, 10, "{i0}\n{i1}");
    assert_eq!(number_after(&id, "cuentas:"), 10, "{id}");
    assert_eq!(reparto(&id), format!("[{r0}, {r1}]"), "{id}\n{i0}\n{i1}");
    for i in [&i0, &i1] {
        assert_eq!(
            number_after(i, "traspasos de jugador sin acuse:"),
            0,
            "quedaron traspasos sin cerrar:\n{i}"
        );
    }

    // Y si se pierde la carpeta del directorio, se reconstruye a partir de los shards.
    let rb = tsofgw(
        &d,
        &[
            "store",
            "rebuild-directory",
            "--shard-dirs",
            "datos0,datos1",
            "--data-dir",
            "datos-dir2",
            "--cluster-key-file",
            "c.key",
        ],
    );
    let rbt = text(&rb.stdout);
    assert!(rb.status.success(), "{rbt}\n{}", text(&rb.stderr));
    assert!(rbt.contains("10 cuentas"), "{rbt}");
    let id2 = info(&["datos-dir2", "--directory", "--cluster-key-file", "c.key"]);
    assert_eq!(reparto(&id2), reparto(&id), "{id}\n{id2}");
    // No se reconstruye encima de otro directorio: sería pisar lo que sabe.
    let otra = tsofgw(
        &d,
        &[
            "store",
            "rebuild-directory",
            "--shard-dirs",
            "datos0,datos1",
            "--data-dir",
            "datos-dir2",
            "--cluster-key-file",
            "c.key",
        ],
    );
    assert!(!otra.status.success());
}
