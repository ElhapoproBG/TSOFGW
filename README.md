# TSOFGW — The Seed of the Great World

Motor dinámico y open source para mundos inmersivos de realidad virtual a gran escala (MMO).
No es un juego: es la base sobre la que se programa, construida como un **grafo de interdependencias**.

- **Grafo vivo:** todo (simulación, red, física, audio, render, hardware) es un nodo con contrato tipado.
- **Apagado selectivo:** un nodo solo corre si algo activo lo necesita; el resto hiberna. Lo mismo ocurre en el mundo: solo se simula donde hay jugadores cerca.
- **Presupuesto por plataforma:** los perfiles `vr`, `pc` y `consola` fijan cuánto cabe por frame; lo de menor prioridad se descarta.
- **Menos fricción para MMO:** mundo, interés, shards y red como piezas listas para conectar.
- **Actualizaciones por módulo:** hot-swap de nodos con reglas de compatibilidad, módulos con manifiesto, hash y firma.
- **Multiplataforma:** VR primero, con PC y consolas soportados por backends intercambiables.
- **Rust** en el núcleo, **C** de apoyo vía FFI (con respaldo en Rust si no hay compilador de C).

## Crates

| Crate | Función |
|---|---|
| `tsofgw-core` | Grafo, contratos, planificador, hot-swap, DOT |
| `tsofgw-world` | Rejilla espacial, interés, regiones activas, shards |
| `tsofgw-net` | Snapshots delta, seq/ack, predicción |
| `tsofgw-hal` | Interfaces de plataforma, perfiles, backends headless |
| `tsofgw-pack` | SHA-256, manifiestos, confianza, permisos |
| `tsofgw-sys` | FFI a C con respaldo en Rust |
| `tsofgw-cli` | Herramienta `tsofgw` |

## Probar

Requiere Rust 1.90 o superior (`rustup update`). No hace falta compilador de C.

    cargo test --all
    cargo run -p tsofgw-cli -- demo
    cargo run -p tsofgw-cli -- demo --profile pc
    cargo run -p tsofgw-cli -- dot
    cargo run -p tsofgw-cli -- render --scene mixta -o mixta.png
    cargo run --release -p tsofgw-cli -- loadtest --clients 200 --npcs 20000

`loadtest` levanta un servidor y cientos de bots (sobre una red simulada con pérdida o sobre UDP real) y mide tiempo de tick, interés, ancho de banda y convergencia. La demo mide, sobre un MMO mínimo, la simulación selectiva, el interés de los jugadores, el ahorro de red con snapshots delta y qué nodos descarta cada perfil por presupuesto.

## Estado

Versión 0.3.0, prototipo. Incluye un servidor autoritativo con transporte UDP autenticado (sin cifrado todavía), cliente con predicción y pruebas de carga medidas. Dibuja con un rasterizador por software verificado (`tsofgw render`); el backend de GPU con wgpu está escrito en `gpu/` pero aún no compilado con el wgpu real, y OpenXR no está empezado. Lo que está verificado y lo que no (sin sandbox, firmas solo HMAC, sin transporte de red) está en `docs/07-estado-y-pendientes.md`.

## Documentación

1. `docs/01-vision.md` — visión y principios
2. `docs/02-arquitectura.md` — arquitectura, plataformas y hitos
3. `docs/03-modelo-de-amenazas.md` — amenazas, confianza y estado de la seguridad
4. `docs/04-marca-y-certificacion.md` — licencia, marca y certificación
5. `docs/05-experiencia-del-desarrollador.md` — cómo se usa TSOFGW
6. `docs/06-mundo-y-red.md` — mundo, shards y protocolo de red
7. `docs/07-estado-y-pendientes.md` — estado verificado y pendientes
8. `docs/08-red-en-linea.md` — transporte, servidor autoritativo y pruebas de carga

## Licencia

Apache-2.0. Los forks son libres; el nombre "TSOFGW" y el sello de certificación se rigen por `docs/04-marca-y-certificacion.md`.
