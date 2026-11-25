# Cambios

## 0.1.0
**Núcleo**
- Flujo de datos entre nodos (`NodeIo`, `Value`, `fn_node`) y validación de salidas contra el contrato.
- Ejecución paralela por capas de profundidad (`tick_parallel`), idéntica a la secuencial.
- Exportación del grafo a Graphviz (`to_dot`).

**Crates nuevos**
- `tsofgw-world`: rejilla espacial, interés con histéresis, regiones activas, shards con rendezvous hashing.
- `tsofgw-net`: snapshots completos y delta, seq/ack, predicción y reconciliación.
- `tsofgw-hal`: interfaces de gráficos/XR/entrada, perfiles de plataforma, backends headless.
- `tsofgw-pack`: SHA-256, HMAC, manifiestos, niveles de confianza, revocación.
- `tsofgw-cli`: herramienta `tsofgw` (demo, dot, hash, pack, verify, new).

**Portabilidad y calidad**
- `tsofgw-sys` compila con o sin compilador de C: respaldo en Rust puro (`TSOFGW_NO_C=1` lo fuerza; se usa solo en MSVC).
- CI en Linux, Windows y macOS, y una ejecución sin C. `rust-version` 1.90.
- 64 pruebas; `cargo fmt` y `cargo clippy` sin avisos.

**Corrección**
- `hash.c`: la constante inicial de FNV-1a tenía un dígito de menos; ahora coincide con FNV-1a real.

## 0.0.2
- Contratos tipados, costos declarados con presupuesto y hot-swap de nodos (H1).

## 0.0.1
- Grafo con apagado selectivo y FFI a C.
