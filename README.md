# TSOFGW — The Seed of the Great World

Motor dinámico y open source para mundos inmersivos de realidad virtual a gran escala (MMO).
No es un juego: es la base sobre la que se programa, construida como un **grafo de interdependencias**.

> ## ⚠️ No apto para exponerse a internet
> *Not suitable for exposure to the internet.* TSOFGW 0.4 es un prototipo para desarrollo, pruebas y redes de confianza:
>
> - **La red no está cifrada**, ni la de jugadores ni el bus entre shards: los paquetes se autentican, pero se pueden leer.
> - **Quien tenga la clave de clúster puede hacerse pasar por cualquier shard**, y quien tenga la clave maestra, por cualquier cuenta.
> - **No hay persistencia**: un shard reiniciado pierde su estado, y un traspaso perdido en una partición larga puede perder a un jugador.
> - **No hay directorio de sesiones**: una cuenta podría estar en dos shards a la vez.
> - Nada de esto ha pasado una auditoría externa.
>
> Úsalo en bucle local o en una red privada. Qué protege y qué no, cómo usarlo con seguridad y el plan: [`SECURITY.md`](SECURITY.md).

- **Grafo vivo:** todo (simulación, red, física, audio, render, hardware) es un nodo con contrato tipado.
- **Apagado selectivo:** un nodo solo corre si algo activo lo necesita; el resto hiberna. Lo mismo ocurre en el mundo: solo se simula donde hay jugadores cerca.
- **Presupuesto por plataforma:** los perfiles `vr`, `pc` y `consola` fijan cuánto cabe por frame; lo de menor prioridad se descarta.
- **Menos fricción para MMO:** mundo, interés, shards y red como piezas listas para conectar.
- **Escala:** la replicación del tick se reparte entre hilos con resultado idéntico, hay niveles de detalle por distancia y presupuesto de ancho de banda, y el mundo se reparte entre servidores (clúster de shards con entidades fantasma y traspaso de jugadores entre procesos).
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
| `tsofgw-transport` | UDP autenticado: handshake con cookie, MAC por paquete, anti-replay, fragmentación |
| `tsofgw-online` | Servidor autoritativo, cliente con predicción, bots, banco de pruebas y clúster de shards |
| `tsofgw-soft` | Rasterizador por software, PNG y comparación de imágenes |
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
    cargo run --release -p tsofgw-cli -- loadtest --clients 500 --threads 2 --lod
    cargo run --release -p tsofgw-cli -- loadtest --shards 2x2 --clients 400 --npcs 40000 --patrol 200
    cargo run --release -p tsofgw-cli -- loadtest --shards 2x2 --clients 100 --udp

`loadtest` levanta un servidor y cientos de bots (sobre una red simulada con pérdida o sobre UDP real) y mide tiempo de tick, interés, ancho de banda y convergencia; con `--shards` reparte el mundo entre varios shards y comprueba que nada se pierde ni se duplica al cruzar bordes. La demo mide, sobre un MMO mínimo, la simulación selectiva, el interés de los jugadores, el ahorro de red con snapshots delta y qué nodos descarta cada perfil por presupuesto.

## Estado

Versión 0.4.3, prototipo (**no apto para exponerse a internet**, ver arriba y `SECURITY.md`). Incluye un servidor autoritativo con transporte UDP autenticado (sin cifrado todavía), cliente con predicción, replicación en varios hilos con niveles de detalle y presupuesto de ancho de banda, y un **clúster de shards** (el mundo repartido entre procesos, con entidades fantasma, traspaso de NPC y de jugadores y un bus UDP autenticado), todo con pruebas de carga medidas. Dibuja con un rasterizador por software verificado (`tsofgw render`); el backend de GPU con wgpu (`gpu/`) compila y dibuja lo mismo que el rasterizador por software, comprobado en la integración continua con Vulkan por software (todavía no en una GPU real); OpenXR no está empezado. Lo que está verificado y lo que no (sin sandbox, firmas solo HMAC, sin cifrado, sin persistencia, medido solo en bucle local) está en `docs/07-estado-y-pendientes.md`; el estado de la integración continua (Linux, Windows y macOS) se ve en la pestaña *Actions* del repositorio.

## Documentación

1. `docs/01-vision.md` — visión y principios
2. `docs/02-arquitectura.md` — arquitectura, plataformas y hitos
3. `docs/03-modelo-de-amenazas.md` — amenazas, confianza y estado de la seguridad
4. `docs/04-marca-y-certificacion.md` — licencia, marca y certificación
5. `docs/05-experiencia-del-desarrollador.md` — cómo se usa TSOFGW
6. `docs/06-mundo-y-red.md` — mundo, shards y protocolo de red
7. `docs/07-estado-y-pendientes.md` — estado verificado y pendientes
8. `docs/08-red-en-linea.md` — transporte, servidor autoritativo, escala y pruebas de carga
9. `docs/09-cluster-de-shards.md` — un mundo repartido entre varios servidores
10. `SECURITY.md` — qué protege hoy el motor, qué no y el plan de seguridad

## Licencia

Apache-2.0. Los forks son libres; el nombre "TSOFGW" y el sello de certificación se rigen por `docs/04-marca-y-certificacion.md`.
