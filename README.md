# TSOFGW — The Seed of the Great World

Motor dinámico y open source para mundos inmersivos de realidad virtual a gran escala (MMO).
No es un juego: es la base sobre la que se programa, construida como un **grafo de interdependencias**.

- **Grafo vivo:** todo (simulación, red, física, audio, render, hardware) es un nodo con contrato.
- **Apagado selectivo:** un nodo solo corre si algo activo lo necesita; el resto hiberna.
- **Menos fricción para MMO:** cuentas, red y persistencia como nodos listos para conectar.
- **Interoperabilidad:** identidad federada entre juegos TSOFGW.
- **Actualizaciones por módulo:** firmadas y verificables, sin tocar el resto.
- **Rust** en el núcleo, **C** como apoyo vía FFI encapsulado (`tsofgw-sys`).

Estado: prototipo 0.0.1 (grafo con apagado selectivo + FFI a C). Ver `docs/`.

    cargo test
