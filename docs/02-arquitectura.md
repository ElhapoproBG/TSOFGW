# Arquitectura

## Mapa de crates

| Crate | Qué hace | Estado |
|---|---|---|
| `tsofgw-core` | Grafo de nodos: contratos tipados, flujo de datos, apagado selectivo, presupuesto, ejecución paralela por capas, hot-swap, exportación DOT | Implementado y probado |
| `tsofgw-world` | Rejilla espacial 3D, consultas por radio, gestión de interés con histéresis, regiones activas, reparto en shards (rendezvous hashing) | Implementado y probado |
| `tsofgw-net` | Codificación binaria, snapshots completos y delta, cabecera con seq/ack, predicción y reconciliación | Implementado y probado (sin transporte) |
| `tsofgw-hal` | Interfaces de gráficos, XR y entrada; perfiles `vr`, `vr-120`, `pc`, `consola`; backends headless | Interfaces y headless; sin backends reales |
| `tsofgw-pack` | SHA-256, HMAC, manifiestos de módulo, niveles de confianza, política de permisos, revocación | Implementado; firmas asimétricas pendientes |
| `tsofgw-sys` | FFI a C con respaldo en Rust puro | Implementado |
| `tsofgw-cli` | Herramienta `tsofgw`: demo, dot, hash, pack, verify, new | Implementado |

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
El mismo grafo se particiona: el planificador decide qué subgrafo corre en cada servidor y en el visor. El mundo, la red y los shards están descritos en el documento 06.

## Hitos
- **H0 (hecho):** grafo + apagado selectivo + FFI a C.
- **H1 (hecho):** contratos tipados, costos declarados con presupuesto, hot-swap.
- **H1.5 (hecho, versión 0.1.0):** flujo de datos entre nodos, ejecución paralela por capas, exportación DOT, mundo con interés y shards, red con snapshots delta, HAL con perfiles y backends headless, módulos con manifiesto y confianza, herramienta `tsofgw`, demo medible, CI.
- **H2 (pendiente):** backends reales: wgpu (PC) y OpenXR (VR); sandbox WebAssembly; firmas Ed25519; transporte de red real con cifrado.
- **H3 (pendiente):** inspector gráfico en vivo; ejecución distribuida del grafo entre servidores; entidades fantasma en fronteras de shard.
- **H4 (pendiente):** identidad federada entre juegos, interfaz HAL congelada para backends de terceros, certificación.

El estado detallado, con lo que está verificado y lo que no, está en el documento 07.
