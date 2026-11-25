//! Compila la librería C de apoyo si hay un compilador compatible; si no, el crate usa
//! una implementación equivalente en Rust (cfg `tsofgw_no_c`). Así el proyecto compila en
//! cualquier sistema (p. ej. Windows con el toolchain MSVC, que no trae `gcc`).
//!
//! Variable `TSOFGW_NO_C=1`: fuerza el respaldo en Rust.

use std::{env, path::Path, process::Command};

fn run(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn try_c(out: &Path) -> bool {
    // Un objeto compilado con gcc/clang no se puede enlazar de forma fiable en el target MSVC.
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        return false;
    }
    let obj = out.join("hash.o");
    let lib = out.join("libtsofgw_c.a");
    let (obj_s, lib_s) = (
        obj.to_string_lossy().into_owned(),
        lib.to_string_lossy().into_owned(),
    );
    let mut candidates: Vec<String> = env::var("CC").ok().into_iter().collect();
    candidates.extend(["cc", "gcc", "clang"].map(String::from));
    for cc in candidates {
        if run(&cc, &["-O2", "-fPIC", "-c", "c/hash.c", "-o", &obj_s])
            && run("ar", &["crs", &lib_s, &obj_s])
        {
            return true;
        }
    }
    false
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(tsofgw_no_c)");
    println!("cargo:rerun-if-changed=c/hash.c");
    println!("cargo:rerun-if-env-changed=TSOFGW_NO_C");
    println!("cargo:rerun-if-env-changed=CC");

    let out = std::path::PathBuf::from(env::var("OUT_DIR").unwrap());
    if env::var("TSOFGW_NO_C").is_ok() || !try_c(&out) {
        println!("cargo:rustc-cfg=tsofgw_no_c");
        return;
    }
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=tsofgw_c");
}
