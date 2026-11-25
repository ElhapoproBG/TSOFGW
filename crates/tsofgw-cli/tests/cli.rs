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
