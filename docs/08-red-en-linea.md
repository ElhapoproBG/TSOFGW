# Red en línea: transporte, servidor autoritativo y pruebas de carga

Esta capa (introducida en 0.3.0 y ampliada en 0.4.0 y 0.5.0) convierte los bloques del documento 06 en algo que se puede ejecutar de extremo a extremo: un servidor, clientes y bots hablando por UDP real, o por una red simulada con pérdida y latencia para pruebas reproducibles. La versión 0.4.0 añade el reparto del trabajo entre hilos, los niveles de detalle, el presupuesto de ancho de banda y el clúster de shards (documento 09). La 0.5.0 añade la persistencia, el directorio de sesiones y el cierre limpio (documento 10).

```
 GameClient / Bot ─┐                                       ┌─ GameServer
  predicción        │  mensajes de juego (tsofgw-online)    │  inputs con créditos, NPC por regiones activas,
  reconciliación    │  JOIN · WELCOME · INPUT · SNAPSHOT    │  interés, snapshots delta
 ───────────────────┤                                       ├──────────────────────────────────────────────
  SecureClient      │  transporte seguro (tsofgw-transport) │  SecureServer
  handshake, MAC,   │  HELLO/CHALLENGE/AUTH/ACCEPT · DATA   │  cookies, sesiones, anti-replay, límites
 ───────────────────┤                                       ├──────────────────────────────────────────────
  UdpLink / SimLink ┴───────────── datagramas ──────────────┴─ UdpLink / SimLink
```

## 1. Transporte seguro (`tsofgw-transport`)

**Garantiza**
- **Autenticación mutua.** Cada cuenta tiene una clave derivada de una clave maestra (`Authority::issue`). El cliente demuestra que la posee y el servidor demuestra que también la conoce.
- **Integridad y autenticidad por paquete:** HMAC-SHA256 truncado a 128 bits, con una clave distinta por sentido y por sesión.
- **Anti-replay:** ventana de 128 contadores; un paquete capturado y reenviado no se ejecuta dos veces.
- **Resistencia a suplantación de origen y a amplificación:** el servidor responde a HELLO con una *cookie* atada a la dirección de origen y no reserva nada hasta recibir un AUTH válido. Ninguna respuesta previa a AUTH es mayor que lo que la provoca (HELLO ≥ 64 B, CHALLENGE 17 B, ACCEPT 37 B < AUTH 73 B).
- **Una cuenta, una sesión.** Si la misma cuenta se conecta otra vez, la sesión nueva reemplaza a la vieja, pero solo cuando la nueva envía su primer paquete válido; repetir un AUTH capturado no expulsa a nadie.
- **Fragmentación** de mensajes hasta ~18 KB (16 fragmentos de ≤ 1200 B) con reensamblado acotado en memoria y caducidad.
- Timeouts, paquetes de mantenimiento, límite de paquetes por segundo y por sesión, límite de sesiones.

**No garantiza (todavía)**
- **Secreto hacia adelante y confianza en el cifrado:** desde la 0.6 la carga de cada paquete va cifrada con ChaCha20-Poly1305 (documento 11), pero es una implementación propia **sin auditar**, y las claves de sesión salen de la clave de la cuenta y dos nonces: si esa clave se filtra, se descifran las capturas. El handshake va en claro (la cuenta es visible).
- **Credenciales asimétricas:** quien tenga la clave maestra puede suplantar a cualquier cuenta. Es un esquema para desarrollo y redes privadas.
- **Migración de dirección:** si cambia la IP o el puerto del cliente, debe reconectar.
- **Entrega fiable ni orden:** es UDP con autenticidad. La capa de juego lo tolera (inputs redundantes, snapshots delta con línea base confirmada).

**Entropía.** `OsEntropy` usa `/dev/urandom` (Linux y macOS) o `BCryptGenRandom` (Windows). `DeterministicEntropy` es predecible y existe solo para pruebas.

## 2. Servidor autoritativo (`tsofgw-online`)

- **El cliente no envía posiciones, solo intenciones** (una dirección por tick). La velocidad máxima la impone la función de movimiento, compartida por cliente y servidor, en enteros y determinista. Se verificó exhaustivamente sobre las 16 777 216 direcciones posibles que ninguna supera el paso máximo.
- **Créditos de input:** cada tick el jugador gana un crédito (máximo 3) y cada input aplicado cuesta uno. Inundar al servidor de inputs no da velocidad extra: en la prueba, un cliente que envía 400 inputs por tick avanza lo mismo que uno honesto. La cola es acotada y los inputs repetidos o viejos se descartan.
- **Mensajes malformados:** se cuentan y quien supera el límite es expulsado y retirado del mundo.
- **Apagado selectivo de NPC:** solo se simulan los que están en regiones cercanas a algún jugador. En la prueba de carga estándar se simula entre el 24 % y el 28 % de los NPC; sin jugadores, ninguno.
- **Interés:** cada jugador recibe lo que está dentro de su radio (con histéresis), con un tope de 256 entidades (las más cercanas).
- **Snapshots delta:** contra la última línea base que el propio jugador confirmó; si se perdió o es muy vieja, el servidor se repliega solo a un snapshot completo. Las confirmaciones viajan en la cabecera seq/ack de cada mensaje y solo se confirma lo que se decodificó con éxito.

### Escala dentro de un servidor (0.4.0)

El tick tiene dos partes: simular (inputs y NPC) y **replicar** (por cada jugador: interés → snapshot → codificación). Lo segundo es lo que crece con los jugadores y es independiente entre jugadores, así que se reparte:

- **Replicación en paralelo (`--threads N`).** 1 = un solo hilo (por defecto en `loadtest`), 0 = los núcleos disponibles (tope 16; es lo que usa `serve`). Cada hilo toma jugadores distintos y los resultados se recogen **en el orden de los jugadores**; el envío (firma + red) sigue en un solo hilo. **No cambia el resultado**: una prueba compara la huella de *todos* los paquetes que cruzan la red con 1, 2, 3 y 8 hilos, y es idéntica (también con LOD y presupuesto activos).
- **Vecindad compartida (`--share-min N`, 8 por defecto).** Si en una celda hay al menos N jugadores (una multitud), su vecindad se reúne una sola vez y se comparte en lugar de consultarse por jugador. Tampoco cambia ni un byte (hay una prueba con 0, 2 y 4 hilos). Para probar multitudes: `--spawn-half 30` hace que todos aparezcan en un área de ±30 u.
- **Niveles de detalle por distancia (`--lod`).** Lo cercano (hasta el 50 % del radio de interés) se actualiza en cada tick (20 Hz), hasta el 80 % cada 2 ticks (10 Hz) y el resto cada 4 (5 Hz). Lo que se sujeta es **el último valor que se *envió* a ese cliente**, no el de la línea base, así que el cliente reconstruye exactamente lo que el servidor registró; lo que el cliente aún no tiene se envía siempre y **el propio jugador siempre va al día**. Con el mundo quieto, el estado final es idéntico al de no usar LOD.
- **Presupuesto de ancho de banda (`--budget B`).** Máximo de bytes por snapshot codificado (64–60 000; 0 = sin límite). Lo que no cabe se **retrasa, lo más lejano primero** (el propio jugador va siempre primero), nunca se descarta: llega en snapshots siguientes. El tamaño de cada candidato se calcula sin codificarlo (`varint_len`/`zigzag_len`), con una única codificación al final; una prueba aleatoria (400 rondas × 11 presupuestos) comprueba que el resultado es idéntico al de la versión lenta que codifica cada candidato.

## 3. Cliente (`GameClient`)
- **Predicción:** aplica sus inputs al instante y, al llegar el estado del servidor, reaplica los pendientes. Cada input se reenvía en los 4 mensajes siguientes para tolerar pérdidas sin retransmisión.
- **Interpolación:** `view(now)` dibuja a los demás entre los dos snapshots que rodean `now` menos 2 ticks, y a mi jugador con la posición predicha.
- **Cambio de shard (0.4.0):** en un clúster, el servidor puede redirigir al cliente a otro shard (documento 09). El cliente abre una segunda conexión con su propia clave de cuenta mediante un `Connector` y, cuando el nuevo shard lo recibe, cambia sin perder predicción ni vista. Como cada shard cuenta sus ticks desde que arrancó, al cambiar se descarta el historial de snapshots para no tratar como «viejo» lo que simplemente tiene otra numeración.
- **Redirección al entrar (0.5.0):** con persistencia, un shard que no es el dueño de la cuenta contesta al `JOIN` con una redirección (si todavía no puede decidir, por ejemplo porque falta la respuesta del directorio, el cliente simplemente repite su `JOIN`); el cliente con `Connector` la sigue solo (`login_redirects`, que no cuenta como cambio de shard) y los bots lo muestran como «entradas redirigidas». Una cuenta nueva se crea en el shard que posee su punto de aparición.
- **Cierre limpio (0.5.0):** cuando un servidor se apaga con `Ctrl-C` o `SIGTERM`, expulsa a sus clientes con un aviso (`DISCONNECT`, motivo «Kicked») en lugar de desaparecer: antes, el cliente seguía creyéndose conectado hasta que vencía el tiempo de espera del transporte (5 s). Tras una caída de verdad sigue tardando esos 5 s y **no se reconecta solo**.

## 4. Herramientas

```
tsofgw keygen -o master.key                      # clave aleatoria (no sobrescribe; 0600 en Unix)
tsofgw token --master-file master.key --player 7 # clave de la cuenta 7
tsofgw serve --master-file master.key --bind 0.0.0.0:7777 --npcs 20000 [--threads N] [--lod] [--budget B]
tsofgw bots  --master-file master.key --connect 127.0.0.1:7777 --count 50 --duration 60
tsofgw loadtest --clients 200 --npcs 20000 --seconds 20 [--loss 0.05 --latency 40 --jitter 10] [--udp]
                [--threads N] [--lod] [--budget B] [--spawn-half H] [--share-min N]
tsofgw loadtest --shards 2x2 --clients 200 --patrol 100 [--udp] [--bus-loss P --bus-latency MS --bus-jitter MS]
```
`loadtest` ejecuta servidor y bots en un proceso. Sin `--udp` usa la red simulada, así que el resultado (todo salvo los tiempos de CPU) es reproducible; con `--udp` usa sockets reales de este equipo. Al final deja a los bots quietos y sin pérdida y comprueba que la posición predicha coincide con la del servidor y que la vista del cliente reconstruida desde deltas es idéntica a lo que el servidor envió. Con `--shards CxF` reparte el mundo entre C×F shards y comprueba además que no se perdió ni se duplicó ningún NPC ni jugador. `serve --shard` y `bots --shards` ponen en marcha un clúster de procesos reales (documento 09).

## 5. Mediciones

Entorno: sandbox de **2 núcleos** (con ruido de ±15–30 % de una ejecución a otra: compara solo cifras tomadas una tras otra), compilación `--release`, 20 000 NPC, 2 % de pérdida, 30 ± 10 ms de latencia por sentido, todos los bots caminando, jugadores repartidos en 300 × 300 unidades. Cada cifra es la mediana de 3 ejecuciones de 20 s. Presupuesto de tick: 50 ms (20 Hz).

### Un servidor, un hilo

| Jugadores | Tick medio | Tick máximo | Bajada por cliente* | Subida por cliente* | Convergencia final |
|---:|---:|---:|---:|---:|---|
| 50 | 2,5 ms | 8,7 ms | 87 kbps | 14 kbps | 50/50 exactas |
| 100 | 4,7 ms | 8,5 ms | 91 kbps | 14 kbps | 100/100 exactas |
| 200 | 8,6 ms | 15,1 ms | 94 kbps | 14 kbps | 200/200 exactas |
| 300 | 13,5 ms | 22,4 ms | 97 kbps | 14 kbps | 300/300 exactas |
| 500 | 22,7 ms | 37,9 ms | 99 kbps | 14 kbps | 500/500 exactas |

\* Con cabeceras IPv4+UDP (28 B por paquete).

- Con un solo hilo el servidor cabe en su presupuesto con 500 jugadores densos, incluso en el peor tick medido. Con la 0.3.0 medida a la misma hora, 500 jugadores costaban 44–47 ms de media.
- El coste crece casi linealmente: ~45 µs por jugador y tick. La firma de paquetes y el envío son ~2 ms de cada tick (≈ 10 %): la seguridad no es el cuello de botella; lo es el interés (~46 % de la CPU).
- El tope de 256 entidades visibles está activo en este escenario, por eso la bajada casi no varía con los jugadores.
- El RTT que se informa es el tiempo hasta recibir la confirmación dentro de un snapshot, por lo que incluye la espera al siguiente tick (hasta 50 ms); no es una medida pura de red.

### Hilos

| Jugadores | 1 hilo (medio / máx) | 2 hilos (medio / máx) | Aceleración del tick |
|---:|---|---|---:|
| 200 | 8,6 / 15,1 ms | 6,0 / 12,4 ms | 1,4× |
| 500 | 22,7 / 37,9 ms | 14,8 / 26,9 ms | 1,5× |

Con 2 hilos la replicación pasa de 18,6 ms a 10,7 ms de pared en el caso de 500 jugadores. Lo que sigue en un solo hilo (NPC, envío con firma, inputs) explica por qué no es 2×. **Solo hay 2 núcleos aquí**: el comportamiento con más núcleos no está medido.

### Niveles de detalle y presupuesto (500 jugadores, 1 hilo, medidos uno tras otro)

| Configuración | Tick medio | Bajada* | Qué ocurre |
|---|---:|---:|---|
| sin LOD ni presupuesto | 25–27 ms | 98,5 kbps | referencia (dos mediciones) |
| `--lod` | 28,2 ms | **72,2 kbps (−27 %)** | 73,6 entidades sujetas por snapshot en promedio |
| `--budget 1200` | 24,7 ms | 99,3 kbps | casi nunca recorta (2 205 snapshots en 20 s) |
| `--budget 800` | 33,4 ms | 94,3 kbps | recorta 75 236 snapshots: es trabajo real de elegir qué retrasar |

Con 2 hilos y `--lod`, el tick medio con 500 jugadores es 14,7 ms (14,8 ms sin LOD): el LOD ahorra un 27 % de ancho de banda sin costo medible de CPU. Un presupuesto muy ajustado cuesta CPU (elegir qué retrasar); uno holgado no se nota.

### Multitudes

300 jugadores aparecen en un área de ±30 u (`--spawn-half 30`), 1 hilo: con la vecindad compartida desactivada (`--share-min 0`) el tick medio es **16,0–16,3 ms** (interés 8,5 ms); con ella (`--share-min 8`, el valor por defecto) es **13,3 ms** (interés 5,1 ms). Los bytes de la red son idénticos (137,6 kbps de bajada en ambos).

### Un clúster frente a un servidor

400 jugadores y 40 000 NPC, todos aparecen a ±40 u del punto donde se juntan las cuatro regiones y la mitad patrulla cruzando bordes: el **peor caso para el tráfico de fantasmas**. Simulado en un proceso; los tiempos son los de cada shard.

| | Un servidor | Clúster 2×2 (4 shards) |
|---|---:|---:|
| Tick medio del shard más cargado | 28,2 ms (máx 40,5) | **8,7 ms** (máx 19,1) |
| Presupuesto de 50 ms usado | 56 % | 17 % |
| Suma de los ticks de todos los shards | 28,2 ms | 29,2 ms |
| Jugadores por shard | 400 | 82 · 111 · 87 · 120 |
| Bus entre shards | — | 1,1 MB/s en total, 28 190 mensajes en 24 s |
| Traspasos de jugador | — | 428 de 428 completados, 0 cancelados, 0 cambios fallidos |
| Reparto final | — | 0 NPC y 0 jugadores perdidos o duplicados; 0 entidades que faltaran o sobraran en las vistas |

El trabajo total se conserva (la suma casi no cambia: el costo de los fantasmas es de ~3,5 %), y en máquinas separadas el límite lo marca el shard más cargado: aquí **3,2 veces menos por máquina con 4 shards**. El desequilibrio entre shards (82 frente a 120 jugadores) es el del reparto estático en rejilla (documento 09).

## 6. Cómo se optimizó y qué se aprendió

**0.3.0.** La primera versión gastaba 47 ms por tick con 200 jugadores. El perfil por fases (`loadtest` lo imprime) mostró dónde estaba el tiempo: interés 38 %, construcción de snapshots 33 %, codificación 18 %. Se pasó a 10–14 ms con: hash rápido en los mapas del mundo, listas ordenadas en lugar de conjuntos en el interés, estado cuantizado de los NPC calculado una vez al moverse, construcción de snapshots por lotes y codificación delta en una sola pasada. Cada cambio tiene una prueba que lo compara con la versión simple (consultas contra fuerza bruta, interés contra una referencia con conjuntos durante 300 ticks, `encode_delta` byte a byte).

**0.4.0.** Dos funciones nuevas empezaron costando más CPU de la que ahorraban en red, y el perfil por fases lo mostró antes de darlas por buenas:
- **LOD:** la fase de construcción de snapshots pasó de 4,8 a 11,9 ms por tick porque buscaba el valor anterior de cada entidad con una búsqueda binaria. Como las dos listas están ordenadas por id, se recorren a la vez (una pasada): 11,9 → ~7 ms. Queda ~3 ms por encima de no usar LOD, que se compensan en parte con menos codificación y envío; el tick total solo sube 1–3 ms (en el ruido de la medición) y con 2 hilos no se distingue.
- **Presupuesto:** para saber cuántos cambios caben, cada intento reconstruía y codificaba el snapshot (búsqueda binaria sobre el tamaño). Ahora el tamaño exacto de cada candidato se calcula sin codificar (`varint_len`/`zigzag_len`) y solo se codifica una vez; la fase de codificación bajó de 16,6 a 11–12 ms con el presupuesto más ajustado. Una prueba aleatoria de 400 rondas × 11 presupuestos comprueba que el resultado es idéntico al de la versión lenta.

## 7. Defectos encontrados al construirlo
- **Movimiento:** al normalizar y escalar en dos pasos, una diagonal podía avanzar un 0,2 % más que un eje. La prueba exhaustiva lo detectó; ahora se calcula en un solo paso redondeando el denominador hacia arriba.
- **Memoria en `Endpoint`:** los paquetes perdidos nunca se olvidaban, así que con pérdida sostenida el conjunto de paquetes en vuelo crecía hasta ~65 000 entradas por conexión. Ahora se descartan los de más de 64 puestos atrás.
- **Clientes que no se enteraban de un cierre limpio (0.5.0):** una prueba que reinicia todos los shards a la vez y espera que todos los clientes sigan jugando mostró 23 de 24: el servidor se iba sin avisar y el cliente esperaba los 5 s del transporte. El apagado ordenado ahora los expulsa con aviso (documento 10, sección 10).
- **Cliente congelado tras cambiar de shard (0.4.0):** al pasar a un shard con la cuenta de ticks atrasada, el cliente descartaba todos sus snapshots como «viejos». Lo encontró una prueba endurecida a propósito (documento 09, sección 8).

## 8. Pendiente
- Sustituir el cifrado propio por una biblioteca auditada, intercambio de claves efímero (secreto hacia adelante) y credenciales asimétricas (Ed25519) emitidas por la autoridad.
- **Simulación de NPC en paralelo** (hoy solo la replicación usa varios hilos) y medir el escalado con más de 2 núcleos.
- **Migración de dirección** del cliente y control de congestión.
- **Reconexión automática de los clientes** tras una caída de un shard (hoy solo la hacen los bancos de pruebas) y **checkpoints incrementales** para mundos grandes (documento 10, sección 11).
- Probar el clúster sobre UDP real también en Windows y macOS (la integración continua ya ejecuta allí las 228 pruebas, incluida la entropía de Windows por FFI a `BCryptGenRandom`, pero la prueba de humo del clúster por UDP solo corre en Linux).
- Medir en hardware real y con latencias de internet reales; todo lo anterior es simulación y bucle local.
