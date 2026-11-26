# Arquitectura

## Mapa de crates

| Crate | Qué hace | Estado |
|---|---|---|
| `tsofgw-core` | Grafo de nodos: contratos tipados, flujo de datos, apagado selectivo, presupuesto, ejecución paralela por capas, hot-swap, exportación DOT | Implementado y probado |
| `tsofgw-world` | Rejilla espacial 3D, consultas por radio, gestión de interés con histéresis, regiones activas, reparto en shards (rendezvous hashing) | Implementado y probado |
| `tsofgw-net` | Codificación binaria, snapshots completos y delta, cabecera con seq/ack, predicción y reconciliación | Implementado y probado (sin transporte) |
| `tsofgw-hal` | Interfaces de gráficos, XR y entrada; perfiles `vr`, `vr-120`, `pc`, `consola`; backends headless | Interfaces y headless; sin backends reales |
| `tsofgw-aead` | ChaCha20-Poly1305 (RFC 8439), propio, sin dependencias | Implementado y probado contra OpenSSL; **sin auditar** (0.6.0) |
| `tsofgw-transport` | Transporte UDP cifrado y autenticado: handshake con cookie, ChaCha20-Poly1305 por paquete, anti-replay, fragmentación, sesiones, red simulada | Implementado y probado; cifrado propio sin auditar, sin secreto hacia adelante |
| `tsofgw-online` | Protocolo de juego, servidor autoritativo (créditos de input, regiones activas, interés, snapshots delta; replicación en varios hilos, niveles de detalle y presupuesto de ancho de banda), cliente con predicción, bots, **clúster de shards** (fantasmas, traspaso de NPC y de jugadores, bus UDP autenticado), banco de pruebas | Implementado y probado |
| `tsofgw-pack` | SHA-256, HMAC, manifiestos de módulo, niveles de confianza, política de permisos, revocación | Implementado; firmas asimétricas pendientes |
| `tsofgw-sys` | FFI a C con respaldo en Rust puro | Implementado |
| `tsofgw-soft` | Rasterizador por software (recorte, z-buffer, perspectiva), PNG y comparación de imágenes | Implementado y probado |
| `tsofgw-cli` | Herramienta `tsofgw`: demo, dot, render, hash, pack, verify, new, keygen, token, serve (también como shard de un clúster), bots, loadtest | Implementado |

## Capas
1. **Hardware (HAL):** wgpu/Vulkan/Metal/D3D12, OpenXR, audio, entrada. Rust; C vía `tsofgw-sys`.
2. **Grafo (núcleo):** nodos tipados, dependencias explícitas, planificador, estados de nodo.
3. **Nodos estándar:** mundo (ECS/celdas), red, persistencia, cuentas, render, audio.
4. **Juego:** nodos y datos del desarrollador.

## El grafo (`tsofgw-core`)

**Contrato de nodo.** Nombre y versión `(mayor, menor)`; puertos de entrada y salida con tipo (`Unit`, `Int`, `Float`, `Bytes`, `Transform`, `Frame`, `Events`); permisos (`net`, `disk`, `accounts`, `xr`…); costo declarado por tick (CPU y GPU en µs, memoria en KiB); estado transferible.

**Conexiones.** Un nodo solo se conecta a nodos que ya existen, así que no hay ciclos. Al conectar se valida que los puertos existan, que los tipos coincidan y que una entrada no se conecte dos veces.

**Flujo de datos.** Cada tick, un nodo lee sus entradas y escribe sus salidas mediante `NodeIo`. Si publica una salida no declarada o con tipo equivocado, se descarta y se cuenta (`contract_violations`).

**Apagado selectivo.** Un nodo está activo si alguien demandado lo necesita, directa o indirectamente; los demás hibernan y no consumen CPU.

**Presupuesto.** Con un presupuesto por tick, el planificador admite las raíces demandadas por prioridad mientras quepan junto con sus dependencias. Las que no caben quedan en `shed()`. Los perfiles de `tsofgw-hal` fijan el presupuesto según la plataforma y la frecuencia objetivo.

**Ejecución paralela.** Cada nodo tiene una profundidad (1 + la mayor de sus dependencias). Los nodos de la misma profundidad no dependen entre sí, así que `tick_parallel` los ejecuta en varios hilos capa por capa. El resultado es idéntico al de `tick` (hay una prueba que lo comprueba).

**Hot-swap.** `replace` cambia un nodo en caliente conservando id, conexiones y estado. Es compatible si: mismo nombre y versión mayor, versión menor igual o mayor, conserva todas las salidas, no exige entradas nuevas, mantiene las entradas ya conectadas y no pide permisos nuevos (salvo autorización explícita).

**Inspección.** `to_dot()` exporta el grafo a Graphviz: verde = activo, gris = hibernado, rojo = descartado por presupuesto, con costos y nombres de puertos. `tsofgw dot` lo imprime para el grafo de la demo.

## Plataformas (VR primero, sin dejar fuera PC y consolas)
Cada plataforma es un **backend de la capa HAL**, expuesto como nodos intercambiables: el código de juego no cambia, cambian los nodos de salida y entrada.

| Plataforma | Salida / entrada | Notas |
|---|---|---|
| VR (prioridad) | OpenXR, render estéreo | 90–120 Hz estables; presupuesto de latencia estricto |
| PC (Windows/Linux/macOS) | Vulkan / D3D12 / Metal vía wgpu; teclado, ratón, gamepad | Mismo grafo, render en pantalla plana |
| Consolas | SDK propietario de cada fabricante | Ver abajo |

**Perfiles de ejecución.** Un perfil (`vr`, `vr-120`, `pc`, `consola`) fija frecuencia objetivo, presupuesto de frame y de memoria. Los valores actuales son puntos de partida, no mediciones: hay que calibrarlos con hardware real.

**Consolas y código abierto.** Los SDK de consola (PlayStation, Xbox, Nintendo) están bajo NDA y no pueden publicarse. El núcleo y los contratos de backend son abiertos, y los backends de consola los desarrollan y mantienen en privado quienes tengan licencia de desarrollador de cada fabricante, usando la misma interfaz HAL. Apache-2.0 lo permite. Esos backends seguirán la política de firmas y certificación del documento 04.

## Cliente y servidor
El mismo grafo se particiona: el planificador decide qué subgrafo corre en cada servidor y en el visor. El mundo y la red están descritos en el documento 06; la red en línea en el 08, el reparto del mundo entre servidores en el 09 y la persistencia con el directorio de sesiones en el 10. Hoy el servidor autoritativo y el clúster son código de `tsofgw-online` (no nodos del grafo todavía): convertir su canalización en nodos, de modo que se pueda apagar selectivamente cada fase, es trabajo pendiente.

## Hitos
- **H0 (hecho):** grafo + apagado selectivo + FFI a C.
- **H1 (hecho):** contratos tipados, costos declarados con presupuesto, hot-swap.
- **H1.5 (hecho, versión 0.1.0):** flujo de datos entre nodos, ejecución paralela por capas, exportación DOT, mundo con interés y shards, red con snapshots delta, HAL con perfiles y backends headless, módulos con manifiesto y confianza, herramienta `tsofgw`, demo medible, CI.
- **H2a (hecho, versión 0.2.0):** matemática de render (`Mat4`, cámaras, fov asimétrico de XR), escena y mallas, rasterizador por software (`tsofgw-soft`, verificado) con salida PNG y comparación de imágenes, comando `tsofgw render`. Backend de GPU con wgpu (`gpu/`): compila contra wgpu 30.0.2 y su imagen coincide con la del rasterizador por software en la CI (con Vulkan por software; **no probado en una GPU real**).
- **H2c (hecho, versión 0.3.0):** transporte UDP autenticado, servidor autoritativo con cliente y bots, banco de pruebas de carga, optimización guiada por perfil. Ver documento 08.
- **H2d (hecho, versión 0.4.0):** escala. Replicación del tick en varios hilos con resultado idéntico, niveles de detalle y presupuesto de ancho de banda, vecindad compartida para multitudes, y clúster de shards (mundo repartido en rejilla, entidades fantasma, traspaso de NPC y de jugadores, bus UDP autenticado), todo con banco de pruebas y CLI. Ver documentos 08 y 09.
- **H2e (hecho, versión 0.5.0):** persistencia y sesiones. Almacén durable con diario y checkpoints autenticados (`tsofgw-store`), estado de cada shard en disco, directorio de sesiones que decide en qué shard vive cada cuenta, traspasos a prueba de caídas y particiones largas, cierre limpio por señales, y una herramienta que estropea el código a propósito para comprobar que las pruebas lo notan. Ver documento 10.
- **H2b (pendiente):** probar el backend wgpu en una GPU real (hoy solo con Vulkan por software en la CI); ventana y presentación; backend OpenXR; sandbox WebAssembly; firmas Ed25519; cifrado AEAD del transporte.
- **H3 (pendiente):** inspector gráfico en vivo; ejecución distribuida **del grafo** entre servidores (hoy lo que se reparte es el mundo, no los nodos); equilibrado dinámico de regiones en el clúster y un directorio de sesiones replicado.
- **H4 (pendiente):** identidad federada entre juegos, interfaz HAL congelada para backends de terceros, certificación.

El estado detallado, con lo que está verificado y lo que no, está en el documento 07.
