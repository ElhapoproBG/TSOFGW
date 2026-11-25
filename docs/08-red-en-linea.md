# Red en línea: transporte, servidor autoritativo y pruebas de carga

Esta capa (versión 0.3.0) convierte los bloques del documento 06 en algo que se puede ejecutar de extremo a extremo: un servidor, clientes y bots hablando por UDP real, o por una red simulada con pérdida y latencia para pruebas reproducibles.

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
- **Confidencialidad:** los paquetes viajan en claro, autenticados pero no cifrados. Falta un cifrado AEAD de una biblioteca auditada; no se implementó criptografía propia.
- **Credenciales asimétricas ni secreto hacia adelante:** quien tenga la clave maestra puede suplantar a cualquier cuenta. Es un esquema para desarrollo y redes privadas.
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

## 3. Cliente (`GameClient`)
- **Predicción:** aplica sus inputs al instante y, al llegar el estado del servidor, reaplica los pendientes. Cada input se reenvía en los 4 mensajes siguientes para tolerar pérdidas sin retransmisión.
- **Interpolación:** `view(now)` dibuja a los demás entre los dos snapshots que rodean `now` menos 2 ticks, y a mi jugador con la posición predicha.

## 4. Herramientas

```
tsofgw keygen -o master.key                      # clave maestra aleatoria (no sobrescribe; 0600 en Unix)
tsofgw token --master-file master.key --player 7 # clave de la cuenta 7
tsofgw serve --master-file master.key --bind 0.0.0.0:7777 --npcs 20000
tsofgw bots  --master-file master.key --connect 127.0.0.1:7777 --count 50 --duration 60
tsofgw loadtest --clients 200 --npcs 20000 --seconds 20 [--loss 0.05 --latency 40 --jitter 10] [--udp]
```
`loadtest` ejecuta servidor y bots en un proceso. Sin `--udp` usa la red simulada, así que el resultado (todo salvo los tiempos de CPU) es reproducible; con `--udp` usa sockets reales de este equipo. Al final deja a los bots quietos y sin pérdida y comprueba que la posición predicha coincide con la del servidor y que la vista del cliente reconstruida desde deltas es idéntica a lo que el servidor envió.

## 5. Mediciones

Entorno: sandbox de 2 núcleos, **servidor de un solo hilo**, compilación `--release`, 20 000 NPC, 2 % de pérdida, 30 ± 10 ms de latencia por sentido, todos los bots caminando, jugadores repartidos en 300 × 300 unidades.

| Jugadores | Tick medio | Tick máximo | Bajada por cliente* | Subida por cliente* | Convergencia final |
|---:|---:|---:|---:|---:|---|
| 50 | 3,0 ms | 4,4 ms | 83 kbps | 14 kbps | 50/50 exactas |
| 100 | 4,9 ms | 9,5 ms | 88 kbps | 14 kbps | 100/100 exactas |
| 200 | 10,4 ms | 17,5 ms | 92 kbps | 14 kbps | 200/200 exactas |
| 300 | 18,2 ms | 34,7 ms | 95 kbps | 14 kbps | 300/300 exactas |
| 500 | 34,3 ms | 66,4 ms | 97 kbps | 14 kbps | 500/500 exactas |

\* Con cabeceras IPv4+UDP (28 B por paquete). Presupuesto de tick: 50 ms (20 Hz).

Cómo leerlo:
- Con un solo hilo el servidor cabe en su presupuesto hasta unos 600–700 jugadores de esta densidad **en esta máquina**; con 500 el tick máximo ya lo supera. Es el motivo de repartir el trabajo entre hilos y shards, que es el siguiente paso.
- El coste crece casi linealmente con los jugadores (~60–70 µs por jugador y tick). La firma de paquetes y el envío cuestan ~1 ms de cada tick (2–3 %): la seguridad no es el cuello de botella.
- El tope de 256 entidades visibles está activo en este escenario (hay más de 256 a la vista), por eso la bajada casi no varía.
- Con ~257 entidades visibles moviéndose a 20 Hz, un snapshot delta pesa ~450–510 B y uno completo ~1,7–1,9 KB (el delta ocupa 3,5–3,8 veces menos). Es el peor caso: todo lo que se ve se mueve. Un siguiente paso evidente es enviar menos a menudo las entidades lejanas.
- El RTT que se informa es el tiempo hasta recibir la confirmación dentro de un snapshot, por lo que incluye la espera al siguiente tick (hasta 50 ms); no es una medida pura de red.
- Los inputs "repetidos" que informa el servidor son en su mayoría las copias redundantes que el cliente envía a propósito.

**Cómo se optimizó y qué se aprendió.** La primera versión gastaba 47 ms por tick con 200 jugadores. El perfil por fases (`loadtest` lo imprime) mostró dónde estaba el tiempo: interés 38 %, construcción de snapshots 33 %, codificación 18 %. Se pasó a 10–14 ms con: hash rápido en los mapas del mundo, listas ordenadas en lugar de conjuntos en el interés, estado cuantizado de los NPC calculado una vez al moverse, construcción de snapshots por lotes y codificación delta en una sola pasada. Cada cambio tiene una prueba que lo compara con la versión simple (consultas contra fuerza bruta, interés contra una referencia con conjuntos durante 300 ticks, `encode_delta` byte a byte).

## 6. Defectos encontrados al construirlo
- **Movimiento:** al normalizar y escalar en dos pasos, una diagonal podía avanzar un 0,2 % más que un eje. La prueba exhaustiva lo detectó; ahora se calcula en un solo paso redondeando el denominador hacia arriba.
- **Memoria en `Endpoint`:** los paquetes perdidos nunca se olvidaban, así que con pérdida sostenida el conjunto de paquetes en vuelo crecía hasta ~65 000 entradas por conexión. Ahora se descartan los de más de 64 puestos atrás.

## 7. Pendiente
- **Cifrado AEAD** de los datos y credenciales asimétricas (Ed25519) emitidas por la autoridad.
- **Multihilo del tick** (interés, construcción y codificación de snapshots por jugador son independientes) y **entidades fantasma** entre shards.
- **Menor frecuencia para entidades lejanas** y límite de ancho de banda por cliente.
- **Migración de dirección** del cliente y control de congestión.
- Probar en Windows y macOS (la entropía de Windows usa FFI a `BCryptGenRandom` y no se ha podido compilar aquí).
- Medir en hardware real y con latencias de internet reales; todo lo anterior es simulación y loopback.
