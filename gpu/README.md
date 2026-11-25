# Backend de GPU (wgpu)

`tsofgw-wgpu` dibuja con la tarjeta gráfica usando wgpu 30 y puede compararse con el rasterizador por software.
Está fuera del espacio de trabajo principal a propósito: así `cargo test` en la raíz no necesita descargar wgpu.

**Estado:** la primera integración continua lo compiló contra el wgpu real y falló por dos nombres que habían
cambiado en wgpu 30 (`PollType::wait()` → `wait_indefinitely()`, y `get_mapped_range()` ahora devuelve `Result`).
Se corrigieron leyendo las firmas en docs.rs (30.0.2) y se comprobaron contra una maqueta de la API con esas
firmas; **la prueba definitiva es el trabajo `gpu` de la CI** (Vulkan por software con lavapipe), que compila,
dibuja y compara la imagen con la del rasterizador por software.

## Probarlo

Necesita Rust 1.90 o superior y acceso a crates.io. Desde la raíz del proyecto:

    cargo run --release --manifest-path gpu/tsofgw-wgpu/Cargo.toml
    cargo run --release --manifest-path gpu/tsofgw-wgpu/Cargo.toml -- --scene cubo --size 800x600
    cargo test --manifest-path gpu/tsofgw-wgpu/Cargo.toml -- --nocapture

Escribe `gpu.png` y `software.png` en la carpeta actual, imprime el adaptador usado y la diferencia entre ambas.
Termina con `COINCIDEN` si la media de diferencia es menor de 1,5/255 y menos del 2 % de los píxeles difieren más de 16.
Si no hay ningún adaptador gráfico, la prueba se omite con un aviso.

## Si no compila

Los nombres de la API de wgpu cambian entre versiones mayores (este crate fija `wgpu = "30"`). Si sube la
versión, los sitios que más suelen cambiar son `Device::poll`/`PollType`, `Instance::new`/`InstanceDescriptor`,
los campos de `RenderPipelineDescriptor`/`RenderPassDescriptor` y los tipos de retorno de `get_mapped_range` y
`request_adapter`. Las firmas se pueden comprobar en docs.rs y en el ejemplo `hello_triangle` de la versión.

## Qué falta
- Ventana y presentación en pantalla (`winit` + `Surface`).
- Uniformes dinámicos e instancias (hoy se crea un búfer pequeño por objeto y por frame).
- Texturas e iluminación.
