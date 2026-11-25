# Clúster de shards: un mundo repartido entre varios servidores

Esta capa (versión 0.4.0) permite que **un mismo mundo** lo sirvan varios procesos, cada uno dueño de una región. El jugador no debería notar los bordes: ve a quien está al otro lado, los NPC cruzan sin parpadear y su conexión pasa de un shard a otro sin perder estado.

```
   jugadores (UDP autenticado, una clave por cuenta)
        │                       │                       │
 ┌──────┴───────┐        ┌──────┴───────┐        ┌──────┴───────┐
 │   shard 0    │        │   shard 1    │        │   shard 2    │   cada shard = un GameServer completo
 │ región (0,0) │◄──────►│ región (1,0) │◄──────►│ región (0,1) │   + un nodo de clúster
 └──────┬───────┘        └──────┬───────┘        └──────┬───────┘
        └───────── bus entre shards (UDP, HMAC) ─────────┘
          fantasmas · traspaso de NPC · traspaso de jugadores
```

## 1. Cómo se reparte el mundo

- Una **rejilla de `columnas × filas` regiones** (`Partition`) sobre el plano x‑z del mundo de ejemplo (±512 u). Cada región es de un shard (hasta 64 por eje). Las regiones de los bordes exteriores se extienden hasta el infinito, así que cualquier posición pertenece a alguna.
- Un shard es el **mismo `GameServer`** de siempre más un nodo de clúster. Con una rejilla de 1×1 se comporta **idéntico, byte a byte**, a un servidor suelto (hay una prueba que compara la huella de todos los paquetes).
- El reparto es **estático**: no se mueven regiones en caliente para equilibrar la carga. Una multitud concentrada en una región carga a su shard (ver límites).
- `ClusterConfig::validate` rechaza con un motivo claro las configuraciones imposibles: shard que no existe, margen de fantasmas menor que lo que ve un jugador, regiones más pequeñas que el doble del margen, tiempos incoherentes.

Con los valores por defecto el margen de fantasmas es de **100 u** (`max(radio de simulación, radio de interés × histéresis) + 10`), así que cada región debe medir al menos 200 u: en el mundo de ±512 caben hasta 5×5 regiones.

## 2. Fantasmas: ver lo que está al otro lado

Cada tick, un shard copia a sus vecinos las entidades que están a menos de `ghost_margin` de su región: todos los jugadores cercanos y los NPC que algún jugador del vecino podría ver. Esas copias son **de solo lectura**:

- se incluyen en lo que ve un jugador cercano al borde (por eso lo que ve coincide con la verdad del conjunto);
- hacen que los NPC del vecino se despierten cuando un jugador se acerca por el otro lado (el apagado selectivo cruza fronteras);
- **no se simulan**, y **caducan solas** a los 6 ticks si dejan de refrescarse. No hay mensaje de «borrar»: un paquete perdido, un traspaso o un jugador que se aleja se resuelven solos.

Una entidad propia siempre gana a una copia de la misma entidad. Las listas se parten en trozos exactos de modo que **cada mensaje cabe en un solo datagrama** (ver 5).

## 3. Traspaso de NPC

Cuando un NPC se adentra `npc_depth` (1,5 u) en la región vecina, se **congela** y se envía su estado completo: posición exacta, dirección y generador aleatorio. El mensaje se reenvía cada 4 ticks hasta recibir acuse; el receptor recuerda `(origen, id)` durante 1200 ticks para no adoptarlo dos veces si el acuse se pierde. Mientras tanto el emisor lo sigue mostrando como fantasma, así que no parpadea. Ninguna entidad se pierde, y la única duplicación posible (la copia del emisor y la del receptor a la vez) dura lo que tarda en llegar un acuse; las pruebas lo comprueban en cada segundo de la ejecución.

## 4. Traspaso de jugadores

Lo dirige **el cliente**, que es quien tiene la conexión. La autoridad cambia de dueño en **un solo mensaje** (`HANDOVER`); todo lo demás se reintenta y es idempotente.

```
 cliente            shard origen                 shard destino
    │   (se adentra `player_depth` = 6 u en la región vecina)
    │                  │──── PREPARE ───────────────►│ anota que espera a ese jugador
    │                  │◄─── PREPARE_ACK ────────────│
    │◄── REDIRECT ─────│
    │──────────── 2.ª conexión con su clave de cuenta ───────────►│
    │──────────── JOIN ──────────────────────────────────────────►│
    │                  │◄─── ACTIVATE ───────────────│
    │                  │  (se CONGELA el jugador)
    │                  │──── HANDOVER (estado final) ►│ adopta al jugador
    │                  │◄─── HAND_ACK ───────────────│
    │◄──────────── WELCOME ──────────────────────────────────────│   el cliente cambia de shard
```

- El estado que viaja incluye la posición y **el último input procesado**: el cliente reconcilia sin salto (en la prueba sin pérdida hubo **0 correcciones** y la posición predicha nunca avanzó más de un paso normal por tick, 0,376 u, tampoco al cambiar de shard). Al llegar, el jugador recibe créditos de input de recuperación para no perder terreno por el cambio.
- **Histéresis:** `player_depth` (6 u) y un enfriamiento de 40 ticks tras cada traspaso o cancelación impiden que quien oscila sobre el borde salte de un lado a otro (hay una prueba: oscilar no traspasa, alejarse sí).
- **Si el destino no responde** o el cliente no llega, el origen cancela (`ABORT`) pasados 200 ticks y el jugador sigue donde estaba; cuando la red vuelve, el traspaso se reintenta. Un jugador nunca queda sin dueño por una cancelación.
- **Un `JOIN` nuevo** (alguien que entra directamente a ese shard) aparece recortado dentro de la región del shard.
- **Los contadores de tick son de cada proceso** (cada shard cuenta desde que arrancó). El cliente tolera cambiar a un shard cuya numeración va atrás: se descarta su historial de snapshots al cambiar (`just_switched`). Esto fue un defecto real encontrado al probar (ver 8).

## 5. El bus entre shards

Una abstracción (`PeerBus`) con dos implementaciones: `SimBus` (determinista, con latencia, jitter, pérdida y duplicados, para pruebas reproducibles) y **`UdpBus`** (sockets reales). Mensajes:

| Mensaje | Para qué |
|---|---|
| `Ghosts` | jugadores y NPC cercanos al borde del vecino (cada tick) |
| `NpcXfer` / `NpcXferAck` | traspaso de un NPC y su acuse |
| `Prepare` / `PrepareAck` | el destino anota que espera a ese jugador y lo confirma |
| `Activate` | el destino pide el estado final cuando el cliente ya llegó |
| `Handover` / `HandoverAck` | **única** transferencia de autoridad y su acuse |
| `Abort` | el origen cancela un traspaso que no avanza |

**Todo mensaje cabe en un datagrama** (`MAX_PEER_MSG` = 1154 B): no hay fragmentación ni reensamblado entre shards. Las listas de fantasmas se parten en trozos de tamaño exacto (`ghost_chunks`, que calcula las longitudes sin codificar), y `SimBus` descarta y cuenta lo que no cabría para que las pruebas demuestren que todo lo que se envía cabría en UDP.

### Datagrama del bus UDP

```
 0x20 | ver | de (2) | para (2) | contador (8) | reloj ms (8) | config (8) | carga | etiqueta (16)
 └──────────────── 30 B de cabecera ─────────────────┘         ≤ 1154 B        HMAC-SHA256/128
```

La etiqueta se calcula con `bus_key = HMAC(clave_de_cluster, "tsofgw/bus-key/v1")` sobre todo lo anterior. El receptor comprueba **en este orden**, y cada rechazo se cuenta por separado: formato → origen válido y distinto de mí → destino soy yo → etiqueta → resumen de configuración → frescura del reloj (±30 s) → ventana anti‑replay por origen. Además:

- **No responde nunca** a lo que recibe y **solo envía a las direcciones configuradas** (`--peers`): no se le puede usar para amplificar ni para reflejar tráfico.
- El contador parte de `reloj_ms × 1000`, así que **sigue creciendo tras un reinicio** y las capturas antiguas no se repiten.
- **Resumen de configuración** (`ClusterConfig::digest`): una huella de lo que debe ser igual en todos los shards (rejilla, tick, radios, NPC —cuántos, velocidad y semilla— y margen). Un shard con otra configuración es auténtico pero **incompatible**: sus datagramas se rechazan y se cuentan aparte («por otra configuración de mundo»), y no mezcla mundos distintos. Lo que es política local (hilos, límites de jugadores, reintentos…) no cuenta.
- Lectura acotada (8192 datagramas por vuelta) y tolerancia a `ConnectionReset` de Windows.

## 6. Garantías y cómo se comprueban

| Garantía | Prueba |
|---|---|
| Ningún NPC se pierde ni se duplica, y ningún jugador queda sin dueño o con dos (una vez asentada la red); durante el juego **nunca se pierde nada** y un duplicado solo existe esperando un acuse | 2×2 sin pérdida: 32 clientes (24 patrullando por los bordes), 25 comprobaciones durante la ejecución y comprobación final exacta |
| Cada traspaso empezado termina, y cada traspaso es exactamente un cambio de shard en el cliente | ídem: `handoffs == switches`, 0 fallos, 0 cancelados |
| Lo que ve un jugador junto al borde coincide con la verdad del clúster | `view_missing = 0`, `view_phantoms = 0`, error de posición < 1 u |
| Resiste red de jugadores con pérdida y un bus con pérdida, duplicados y desorden | bus con 25 % de pérdida y 25 % de duplicados; otro «muy malo» con 50 % de pérdida, 40 % de duplicados y 120 ms de jitter: nada perdido ni duplicado al final |
| Un bus caído no hace daño | los traspasos se cancelan y los jugadores siguen donde estaban; al volver la red, se completan |
| Mensajes hostiles en el bus no rompen nada | datagramas con basura, orígenes imposibles, listas repetidas o desordenadas, NPC en la lista de jugadores… |
| Un shard solo equivale a un servidor suelto | misma huella de todos los paquetes |
| Determinismo | misma semilla → mismo resultado |
| Los shards no necesitan llevar la misma cuenta de ticks | un shard arranca 3 s tarde; tras cada cambio el tick del cliente queda a ≤ 3 del del servidor en un segundo |
| Configuraciones distintas no se mezclan | la huella cambia con lo que importa y solo con eso |
| **Sobre UDP real** (sockets de este equipo) | 2×2 con 42 traspasos y 0 datagramas rechazados; un intruso que forja datagramas con otra clave: todos rechazados y registrados (0 «mal formados», porque la etiqueta falla antes de interpretar nada); una clave de clúster distinta: los shards no se fían, no se completa ningún traspaso, no se aplica ningún fantasma y **no se pierde nada** |
| **Procesos reales** | una prueba de la CLI lanza 2 procesos `serve --shard` (el segundo arranca más tarde, con otra cuenta de ticks) y 10 bots, 8 de ellos patrullando: todos siguen jugando, 0 cambios fallidos, 0 datagramas rechazados, y cada shard entrega y recibe jugadores. A mano también se probaron 4 procesos en rejilla 2×2 con 40 bots (82 cambios de shard, 0 fallidos) |

## 7. Cómo se pone en marcha

Dos claves, ambas de archivo (nunca por línea de comandos): la maestra de cuentas (la misma que usa un servidor suelto) y la **clave de clúster**, que comparten los shards.

```
tsofgw keygen -o master.key
tsofgw keygen -o cluster.key

# Cuatro shards en rejilla 2x2. --bind: puerto de jugadores; --peers: el BUS de cada shard, en orden.
# La rejilla, la lista de --peers, la clave y los parámetros del mundo (--npcs, --seed, --radius…) deben ser iguales en todos.
tsofgw serve --master-file master.key --bind 0.0.0.0:7000 --shard 0 --grid 2x2 --cluster-key-file cluster.key \
             --peers 127.0.0.1:7100,127.0.0.1:7101,127.0.0.1:7102,127.0.0.1:7103 --npcs 20000
tsofgw serve --master-file master.key --bind 0.0.0.0:7001 --shard 1 ...   # y así hasta el 3

# Bots que siguen los traspasos: --shards lista la dirección de JUGADORES de cada shard, en orden
tsofgw bots --master-file master.key --connect 127.0.0.1:7000 --count 40 --patrol 30 \
            --shards 127.0.0.1:7000,127.0.0.1:7001,127.0.0.1:7002,127.0.0.1:7003

# Todo en un proceso, para medir: simulado (reproducible) o sobre UDP real
tsofgw loadtest --shards 2x2 --clients 200 --npcs 20000 --patrol 100
tsofgw loadtest --shards 2x2 --clients 200 --npcs 20000 --patrol 100 --udp
```

`serve` imprime cada 5 s una línea de clúster: NPC propios, copias, traspasos (completados/empezados, cancelados y llegadas), NPC traspasados y datagramas del bus rechazados o mal formados. Un número de «rechazados» que sube sin parar es una configuración distinta, una clave equivocada, un reloj desfasado o alguien probando.

## 8. Defectos encontrados al construirlo

- **Cliente congelado tras cambiar de shard.** Los contadores de tick son de cada proceso; al pasar a un shard con numeración atrasada el cliente descartaba como «viejos» todos sus snapshots y el mundo se quedaba quieto. La primera versión de la prueba no lo vio porque el estado final lo ocultaba; al endurecerla (comprobar el tick del cliente frente al del servidor un segundo después de cada cambio, con un shard que arranca 3 s tarde) apareció y se corrigió.
- **Oráculo de vistas con tope de entidades:** al comparar lo que ve un cliente con la verdad, el tope de entidades visibles hacía «faltar» miles de entidades lejanas; el oráculo ahora calcula solo hasta donde el snapshot llegó.
- **Prueba intermitente del bus:** un contador derivado del reloj cruzaba un milisegundo entre dos lecturas; ahora la base se fija con el contador real del emisor.

## 9. Límites conocidos (para no confundir intención con realidad)

- **Solo medido en bucle local**: no hay mediciones en una red de verdad ni en varias máquinas. Los tiempos son de un sandbox de 2 núcleos.
- **El bus está autenticado pero no cifrado.** Está pensado para una red privada entre servidores; no expongas sus puertos a internet. Necesita relojes sincronizados (NTP) con menos de 30 s de diferencia.
- **Quien tenga la clave de clúster puede hacerse pasar por cualquier shard** (inyectar fantasmas o traspasos). Es una clave de infraestructura: trátala como tal y rótala si se filtra.
- **No hay un directorio de sesiones entre shards:** cada shard garantiza «una cuenta, una sesión» solo localmente. Una cuenta con credenciales válidas podría estar conectada a la vez en dos shards.
- **Sin persistencia:** un shard que se reinicia pierde su estado, y un `HANDOVER` perdido durante una partición larga de red puede perder a un jugador. Hace falta una capa de persistencia (documento 07).
- **Reparto estático** en rejilla: no hay equilibrado dinámico de carga ni fusión/división de regiones.
- **Memoria por shard proporcional al total de NPC** (cada shard reserva todos los ids aunque solo simule los suyos).
- Un clúster de procesos reales se probó en una sola máquina; falta probar fallos de máquinas (apagar un shard en caliente) y latencias de WAN.
