//! `tsofgw`: herramienta de línea de comandos de TSOFGW.

mod demo;

use std::fs;
use std::path::Path;
use tsofgw_hal::Profile;
use tsofgw_pack::{
    check_manifest, sha256, to_hex, HmacScheme, Manifest, Policy, Trust, TrustStore,
};

const AYUDA: &str = "\
tsofgw — The Seed of the Great World

USO:
  tsofgw version
  tsofgw demo [--profile vr|vr-120|pc|consola] [--entities N] [--players N] [--ticks N] [--seed N]
  tsofgw dot [--profile P]                      Grafo de la demo en formato Graphviz (DOT)
  tsofgw hash <archivo>                         SHA-256 de un archivo
  tsofgw pack <payload> --name N --version X.Y [--caps a,b] [--signer S --key-file K] [-o salida]
  tsofgw verify <manifiesto> <payload> [--signer S --key-file K] [--trust official|certified] [--allow-unsigned]
  tsofgw new <nombre>                           Crea un proyecto nuevo que usa TSOFGW
";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}
fn num<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> Result<T, String> {
    match flag(args, name) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| format!("valor inválido para {name}: {v}")),
    }
}
fn profile(args: &[String]) -> Result<Profile, String> {
    match flag(args, "--profile") {
        None => Ok(Profile::VR),
        Some(n) => Profile::by_name(&n).ok_or(format!(
            "perfil desconocido: {n} (usa vr, vr-120, pc o consola)"
        )),
    }
}
/// Primer argumento posicional (que no sea una opción ni el valor de una opción).
fn positionals(args: &[String]) -> Vec<String> {
    let con_valor = [
        "--profile",
        "--entities",
        "--players",
        "--ticks",
        "--seed",
        "--name",
        "--version",
        "--caps",
        "--signer",
        "--key-file",
        "--trust",
        "-o",
    ];
    let mut out = vec![];
    let mut i = 0;
    while i < args.len() {
        if con_valor.contains(&args[i].as_str()) {
            i += 2;
        } else if args[i].starts_with('-') {
            i += 1;
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}
fn read(path: &str) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("no se pudo leer {path}: {e}"))
}
/// Lee una clave de archivo ignorando saltos de línea finales (los editores los añaden).
fn read_key(path: &str) -> Result<Vec<u8>, String> {
    let mut k = read(path)?;
    while matches!(k.last(), Some(b'\n' | b'\r')) {
        k.pop();
    }
    if k.is_empty() {
        return Err(format!("el archivo de clave {path} está vacío"));
    }
    Ok(k)
}

fn cmd_demo(args: &[String]) -> Result<(), String> {
    let cfg = demo::DemoConfig {
        profile: profile(args)?,
        entities: num(args, "--entities", 50_000)?,
        players: num(args, "--players", 100)?,
        ticks: num(args, "--ticks", 60)?,
        seed: num(args, "--seed", 1)?,
    };
    if cfg.players > cfg.entities {
        return Err("--players no puede superar a --entities".into());
    }
    print!("{}", demo::run_demo(&cfg).render(&cfg));
    Ok(())
}

fn cmd_dot(args: &[String]) -> Result<(), String> {
    let cfg = demo::DemoConfig {
        profile: profile(args)?,
        entities: 100,
        players: 1,
        ticks: 1,
        seed: 1,
    };
    let mut d = demo::build_graph(&cfg);
    d.graph.resolve();
    print!("{}", d.graph.to_dot());
    Ok(())
}

fn cmd_hash(args: &[String]) -> Result<(), String> {
    let p = positionals(args);
    let path = p.first().ok_or("falta el archivo: tsofgw hash <archivo>")?;
    println!("{}  {}", to_hex(&sha256(&read(path)?)), path);
    Ok(())
}

fn parse_version(v: &str) -> Result<(u16, u16), String> {
    let (a, b) = v.split_once('.').ok_or("la versión debe ser X.Y")?;
    Ok((
        a.parse().map_err(|_| "versión inválida")?,
        b.parse().map_err(|_| "versión inválida")?,
    ))
}

fn cmd_pack(args: &[String]) -> Result<(), String> {
    let p = positionals(args);
    let path = p
        .first()
        .ok_or("falta el payload: tsofgw pack <payload> --name N --version X.Y")?;
    let name = flag(args, "--name").ok_or("falta --name")?;
    let version = parse_version(&flag(args, "--version").ok_or("falta --version")?)?;
    let caps_text = flag(args, "--caps").unwrap_or_default();
    let caps: Vec<&str> = caps_text.split(',').filter(|c| !c.is_empty()).collect();
    let mut m = Manifest::new(&name, version, &caps, &read(path)?);
    if let Some(signer) = flag(args, "--signer") {
        let key = read_key(&flag(args, "--key-file").ok_or(
            "--signer requiere --key-file (no se acepta la clave en la línea de comandos)",
        )?)?;
        m.sign(&signer, &HmacScheme::new(&key));
    }
    // valida que lo escrito se pueda volver a leer
    let text = m.to_text();
    Manifest::parse(&text).map_err(|e| format!("manifiesto inválido: {e:?}"))?;
    let out = flag(args, "-o").unwrap_or_else(|| format!("{path}.manifest"));
    fs::write(&out, text).map_err(|e| format!("no se pudo escribir {out}: {e}"))?;
    println!("manifiesto escrito en {out}");
    Ok(())
}

fn cmd_verify(args: &[String]) -> Result<(), String> {
    let p = positionals(args);
    if p.len() < 2 {
        return Err("uso: tsofgw verify <manifiesto> <payload>".into());
    }
    let manifest =
        Manifest::parse(&String::from_utf8(read(&p[0])?).map_err(|_| "el manifiesto no es UTF-8")?)
            .map_err(|e| format!("manifiesto inválido: {e:?}"))?;
    let payload = read(&p[1])?;

    let mut store = TrustStore::new();
    if let Some(signer) = flag(args, "--signer") {
        let key = read_key(&flag(args, "--key-file").ok_or("--signer requiere --key-file")?)?;
        let trust = match flag(args, "--trust").as_deref() {
            None | Some("official") => Trust::Official,
            Some("certified") => Trust::Certified,
            Some(o) => return Err(format!("--trust desconocido: {o}")),
        };
        store.add_signer(&signer, trust, Box::new(HmacScheme::new(&key)));
    }
    let policy = Policy {
        allow_unsigned: has(args, "--allow-unsigned"),
        ..Policy::default()
    };
    match check_manifest(&store, &policy, &manifest, &payload) {
        Ok(l) => {
            println!(
                "OK: {} v{}.{} — confianza {:?}, permisos: {}",
                manifest.name,
                manifest.version.0,
                manifest.version.1,
                l.trust,
                manifest.capabilities.join(", ")
            );
            for w in l.warnings {
                println!("AVISO: {w}");
            }
            Ok(())
        }
        Err(e) => Err(format!("RECHAZADO: {e:?}")),
    }
}

fn valid_project_name(n: &str) -> bool {
    n.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn cmd_new(args: &[String]) -> Result<(), String> {
    let p = positionals(args);
    let name = p.first().ok_or("falta el nombre: tsofgw new <nombre>")?;
    if !valid_project_name(name) {
        return Err(
            "el nombre debe empezar con una letra y usar solo letras, números, '-' o '_'".into(),
        );
    }
    let dir = Path::new(name);
    if dir.exists() {
        return Err(format!("ya existe {name}"));
    }
    fs::create_dir_all(dir.join("src")).map_err(|e| e.to_string())?;
    let w = |rel: &str, body: String| {
        fs::write(dir.join(rel), body).map_err(|e| format!("no se pudo escribir {rel}: {e}"))
    };
    w(
        "Cargo.toml",
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ntsofgw-core = {{ git = \"https://github.com/ElhapoproBG/TSOFGW\" }}\n"
        ),
    )?;
    w(".gitignore", "target/\n".into())?;
    w("src/main.rs", PLANTILLA.into())?;
    println!("proyecto '{name}' creado. Entra con `cd {name}` y ejecuta `cargo run`.");
    Ok(())
}

const PLANTILLA: &str = r#"use tsofgw_core::{fn_node, Contract, DataType, Graph, Link, Value};

fn main() {
    let mut grafo = Graph::new();

    // Un nodo que cuenta ticks y publica el valor.
    let mut n = 0.0;
    let contador = grafo
        .add(
            fn_node(Contract::new("contador", (1, 0)).output("n", DataType::Float), move |io| {
                n += 1.0;
                io.set_output("n", Value::Float(n));
            }),
            &[],
        )
        .unwrap();

    // Un nodo que lo duplica. Solo corre si alguien lo demanda.
    let doble = grafo
        .add(
            fn_node(
                Contract::new("doble", (1, 0)).input("n", DataType::Float).output("y", DataType::Float),
                |io| {
                    let n = io.float("n").unwrap_or(0.0);
                    io.set_output("y", Value::Float(n * 2.0));
                },
            ),
            &[Link { from: contador, output: "n", input: "n" }],
        )
        .unwrap();

    grafo.set_demanded(doble, true);
    for _ in 0..3 {
        grafo.tick();
        println!("doble = {:?}", grafo.output(doble, "y"));
    }
}
"#;

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            print!("{AYUDA}");
            return Ok(());
        }
    };
    match cmd {
        "version" | "--version" | "-V" => {
            println!("tsofgw {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print!("{AYUDA}");
            Ok(())
        }
        "demo" => cmd_demo(rest),
        "dot" => cmd_dot(rest),
        "hash" => cmd_hash(rest),
        "pack" => cmd_pack(rest),
        "verify" => cmd_verify(rest),
        "new" => cmd_new(rest),
        otro => Err(format!("comando desconocido: {otro}\n\n{AYUDA}")),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn opciones_y_posicionales() {
        let a = v(&[
            "archivo.bin",
            "--name",
            "x",
            "--allow-unsigned",
            "-o",
            "salida",
        ]);
        assert_eq!(positionals(&a), vec!["archivo.bin"]);
        assert_eq!(flag(&a, "--name"), Some("x".into()));
        assert!(has(&a, "--allow-unsigned"));
        assert_eq!(flag(&a, "--nada"), None);
        assert_eq!(num::<u32>(&v(&["--ticks", "5"]), "--ticks", 1), Ok(5));
        assert!(num::<u32>(&v(&["--ticks", "x"]), "--ticks", 1).is_err());
    }

    #[test]
    fn perfiles_y_versiones() {
        assert_eq!(profile(&v(&["--profile", "pc"])), Ok(Profile::PC));
        assert!(profile(&v(&["--profile", "zzz"])).is_err());
        assert_eq!(parse_version("1.12"), Ok((1, 12)));
        assert!(parse_version("1").is_err());
        assert!(valid_project_name("mi-juego_2"));
        assert!(!valid_project_name("2juego"));
        assert!(!valid_project_name("../x"));
    }
}
