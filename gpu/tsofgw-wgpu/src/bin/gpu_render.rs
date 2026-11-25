//! Dibuja una escena con la GPU y con el rasterizador por software, guarda las dos imágenes y las compara.
//!
//!     cargo run --release --manifest-path gpu/tsofgw-wgpu/Cargo.toml -- [--scene triangulo|cubo|mixta] [--size AxB]
//!
//! Escribe `gpu.png` y `software.png` en la carpeta actual. Termina con código 2 si no coinciden.

use tsofgw_core::Transform;
use tsofgw_hal::scene::{render_mono, Scene};
use tsofgw_soft::{compare, png, SoftGraphics};
use tsofgw_wgpu::WgpuGraphics;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let name = flag(&args, "--scene").unwrap_or_else(|| "mixta".into());
    let Some(scene) = Scene::demo(&name) else {
        eprintln!("error: escena desconocida: {name} (usa triangulo, cubo o mixta)");
        std::process::exit(1);
    };
    let size = flag(&args, "--size").unwrap_or_else(|| "640x480".into());
    let parsed = size
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
    let Some((w, h)) =
        parsed.filter(|&(w, h)| (16..=4096).contains(&w) && (16..=4096).contains(&h))
    else {
        eprintln!("error: tamaño inválido: {size} (usa por ejemplo 640x480, entre 16 y 4096)");
        std::process::exit(1);
    };

    let mut gpu = match WgpuGraphics::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    println!("adaptador gráfico: {}", gpu.adapter_name());

    let cam = Transform::default();
    let gpu_px = render_mono(&mut gpu, &scene, &cam, w, h).unwrap_or_else(|| {
        eprintln!("error: la GPU no devolvió la imagen");
        std::process::exit(1)
    });
    let soft_px = render_mono(&mut SoftGraphics::new(1, 1), &scene, &cam, w, h)
        .expect("el backend por software siempre entrega la imagen");

    std::fs::write("gpu.png", png::encode_rgba(w, h, &gpu_px))
        .expect("no se pudo escribir gpu.png");
    std::fs::write("software.png", png::encode_rgba(w, h, &soft_px))
        .expect("no se pudo escribir software.png");

    let d = compare::diff(&gpu_px, &soft_px, 16).expect("las imágenes tienen el mismo tamaño");
    println!(
        "escena '{name}' {w}x{h}: diferencia media {:.3}/255, {:.3} % de píxeles con diferencia > 16, máxima {}",
        d.mean_abs,
        d.fraction_over * 100.0,
        d.max_channel
    );
    if d.is_close() {
        println!("COINCIDEN: la GPU y el rasterizador por software dibujan lo mismo (gpu.png y software.png).");
    } else {
        println!("DIFIEREN: revisa gpu.png y software.png.");
        std::process::exit(2);
    }
}
