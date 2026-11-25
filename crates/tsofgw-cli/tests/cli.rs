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
    let dibujados = px.chunks_exact(4).filter(|p| p[..3] != fondo).count();
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
