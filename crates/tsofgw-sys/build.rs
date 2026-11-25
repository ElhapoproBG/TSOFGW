use std::{env, path::PathBuf, process::Command};
fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let obj = out.join("hash.o");
    let lib = out.join("libtsofgw_c.a");
    assert!(Command::new("gcc").args(["-O2", "-fPIC", "-c", "c/hash.c", "-o"]).arg(&obj).status().unwrap().success());
    assert!(Command::new("ar").arg("crs").arg(&lib).arg(&obj).status().unwrap().success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=tsofgw_c");
    println!("cargo:rerun-if-changed=c/hash.c");
}
