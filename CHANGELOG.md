# Cambios

## 0.4.0 — Escala
**Servidor (`tsofgw-online`)**
- **Replicación en paralelo** (`--threads N`, 0 = núcleos disponibles): interés, snapshot y codificación por jugador en varios hilos, recogidos en orden. El resultado es idéntico: la huella de todos los paquetes coincide con 1, 2, 3 y 8 hilos, también con LOD y presupuesto. Con 2 núcleos, 500 jugadores densos pasan de 22,7 ms a 14,8 ms por tick.
- **Niveles de detalle por distancia** (`--lod`): lo lejano se refresca a 10 y 5 Hz sujetando el último valor *enviado*; el propio jugador siempre va al día. −27 % de ancho de banda con 500 jugadores densos.
- **Presupuesto de ancho de banda por snapshot** (`--budget B`): lo que no cabe se retrasa (lo más lejano primero), nunca se pierde; el tamaño de lo recortado se calcula sin codificar.
- **Vecindad compartida** (`--share-min N`): en multitudes (`--spawn-half`) los jugadores de una misma celda comparten una sola consulta de interés; mismos bytes, −18 % de tiempo de tick con 300 jugadores en 60×60 u.
- `tsofgw-net`: `varint_len` y `zigzag_len`. `tsofgw-transport`: `ReplayWindow` pública.

**Clúster de shards (nuevo; documento 09)**
- El mundo se reparte en una rejilla de regiones, una por shard (`Partition`). **Entidades fantasma** de solo lectura en los bordes, **traspaso de NPC** (estado completo, reenvío hasta acuse, sin adopciones dobles) y **traspaso de jugadores** dirigido por el cliente, con una única transferencia de autoridad, histéresis, cancelación y recuperación.
- **Bus entre shards** (`PeerBus`): `SimBus` determinista para pruebas y `UdpBus` real, con HMAC-SHA256/128, anti-replay, frescura de reloj (±30 s), resumen de configuración (shards con otro mundo se rechazan) y sin respuestas (no amplifica). Todo mensaje cabe en un datagrama.
- Cliente: cambio de shard con un `Connector`; tolera la numeración de ticks distinta de cada shard.
- CLI: `serve --shard I --grid CxF --peers … --cluster-key-file K`, `bots --shards … --patrol`, `loadtest --shards CxF [--udp] [--patrol …]`. `loadtest --shards` falla si el reparto final no es exacto (sirve de prueba de humo en la CI), y la ayuda documenta las opciones nuevas.
- Con 400 jugadores y 40 000 NPC, un clúster 2×2 usa 8,7 ms de media por tick en el shard más cargado frente a 28,2 ms de un servidor único (la suma casi no cambia).

**Pruebas:** 226 → ver la cifra final en la verificación; nuevas: reparto sin pérdida, con pérdida, con un bus muy malo, bus caído, mensajes hostiles, un shard ≡ servidor suelto, determinismo, ticks distintos entre shards, resumen de configuración, 3×2 con 60 clientes y 6 000 NPC; bus UDP (formato, anti-replay, reinicio, reloj, flood); clúster sobre UDP real (reparto, intruso, clave distinta); procesos reales por CLI; equivalencia entre hilos, de la vecindad compartida y del recorte con la versión lenta.

**Correcciones**
- **Cliente congelado al cambiar a un shard con la cuenta de ticks atrasada** (descartaba todos sus snapshots). Lo encontró una prueba endurecida a propósito.
- Dos optimizaciones del propio 0.4.0 empezaron costando CPU y se corrigieron tras medirlas: el LOD (recorrido en paralelo de dos listas ordenadas en lugar de una búsqueda por entidad) y el presupuesto (tamaños exactos sin recodificar).
- La ayuda de `tsofgw` ahora documenta `serve --shard` y `bots --shards`.

**Límites conocidos:** sin cifrado (jugadores ni bus), sin persistencia (un shard reiniciado pierde su estado), reparto estático, sin directorio de sesiones entre shards, la clave de clúster es un punto único de confianza, el clúster solo se probó en una máquina (bucle local), todo medido en un sandbox de 2 núcleos; la aleatoriedad en Windows (FFI a `BCryptGenRandom`) no se pudo compilar aquí.

## 0.3.0
**Red en línea**
- `tsofgw-transport` (nuevo): UDP autenticado con handshake HELLO/CHALLENGE/AUTH/ACCEPT y cookie atada a la dirección, MAC por paquete (HMAC-SHA256/128) con clave por sentido, ventana anti-replay de 128, fragmentación hasta ~18 KB, sesiones con timeout y mantenimiento, límites por sesión; `UdpLink` y una red simulada determinista (`SimNet`: latencia, jitter, pérdida, duplicados). Sin cifrado todavía (ver documento 08).
- `tsofgw-online` (nuevo): protocolo de juego, `GameServer` autoritativo (créditos de input, cola acotada, expulsión por basura, NPC solo en regiones activas, interés con tope, snapshots delta con repliegue), `GameClient` con predicción, reconciliación e interpolación, `Bot` y un banco de pruebas con informe medido.
- `tsofgw-cli`: `keygen`, `token`, `serve`, `bots`, `loadtest` (simulado o `--udp`). Las opciones desconocidas ahora se rechazan en vez de ignorarse.

**Rendimiento (tick con 200 jugadores y 20 000 NPC: de 47 ms a 10–14 ms)**
- `tsofgw-world`: hash rápido en mapas internos (`FastMap`, `CellSet`), `query_radius_d2`, `Interest` sobre listas ordenadas (`visible_ids` sin copia).
- `tsofgw-net`: `encode_delta` en una sola pasada (mismos bytes que antes, comprobado).
- `tsofgw-pack`: `HmacKey` con estados precalculados y `ct_eq` público.

**Correcciones**
- `tsofgw-net`: `Endpoint` ya no acumula indefinidamente los paquetes perdidos (hasta 65 536 entradas por conexión).
- Movimiento: una diagonal podía avanzar un 0,2 % más que un eje; ahora el paso máximo está verificado sobre las 16,7 millones de direcciones.

**Límites conocidos:** sin cifrado, un solo hilo en el servidor (500 jugadores densos superan el presupuesto de tick en el entorno de pruebas), la aleatoriedad en Windows (FFI a `BCryptGenRandom`) no se pudo compilar aquí.

## 0.2.0
**Render**
- `tsofgw-hal`: `math` (Mat4 por filas, cuaterniones, cámaras con fov simétrico y asimétrico para XR) y `scene` (mallas, escenas de demostración, `render_mono` y `render_stereo`). `GraphicsBackend` ahora recibe mallas, cámara y órdenes de dibujo, y puede devolver los píxeles.
- `tsofgw-soft` (nuevo): rasterizador por software con recorte, z-buffer y colores con perspectiva; codificador/decodificador PNG; comparación de imágenes.
- `tsofgw render`: dibuja una escena y escribe un PNG. Ejemplos en `docs/img`.

**GPU (sin verificar)**
- `gpu/tsofgw-wgpu`: backend fuera de pantalla con wgpu 30, shader WGSL y binario `gpu_render` que compara GPU contra software. Escrito sin poder compilarlo con el wgpu real; ver `gpu/README.md`.
- 94 pruebas (con y sin C); `cargo fmt` y `cargo clippy` sin avisos.
- CI: trabajo `gpu` que lo compila y prueba con Mesa lavapipe.

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
