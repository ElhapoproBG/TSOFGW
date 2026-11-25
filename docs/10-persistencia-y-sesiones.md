# Persistencia y directorio de sesiones

Esta capa (versión 0.5.0) resuelve lo que más dolía del clúster de la 0.4: un shard que se reiniciaba perdía su
estado, un traspaso perdido durante una partición larga podía perder a un jugador, y nada impedía que una cuenta
estuviera en dos shards a la vez. Ahora cada shard guarda su estado en disco, un **directorio de sesiones**
decide en qué shard vive cada cuenta, y la regla que lo une todo es una sola:

> **Tras cualquier caída hay un dueño por cada cuenta y por cada NPC: nunca cero, nunca dos.**

```
   jugadores (UDP autenticado)
        │                    │
 ┌──────┴───────┐     ┌──────┴───────┐
 │   shard 0    │     │   shard 1    │          cada shard escribe antes de prometer
 │  [disco 0]   │     │  [disco 1]   │
 └──────┬───────┘     └──────┬───────┘
        │   bus entre shards (UDP, HMAC)
        └─────────┬──────────┘
           ┌──────┴───────┐
           │  directorio  │   un proceso: dice en qué shard vive cada cuenta
           │  [disco D]   │   y anota cada cambio de dueño antes de responder
           └──────────────┘
```

> ⚠️ **Los datos en disco están autenticados, no cifrados**, y la red sigue sin cifrar (README y `SECURITY.md`).
> Quien lea la carpeta de datos ve las cuentas y sus posiciones; quien la modifique sin la clave es detectado, pero
> **restaurar una copia vieja de la carpeta no se detecta**.

## 1. Qué garantiza y qué no

**Con persistencia y directorio, las pruebas comprueban que:**

- una cuenta no se ve residente en dos shards en ningún momento (se vigila cada 100 ms simulados durante toda la marcha,
  no solo al final; un vistazo cada 100 ms no demuestra que no ocurra entre dos vistazos);
- un traspaso empezado termina aunque caigan el origen, el destino, los dos o el directorio, en cualquiera de los tres
  momentos delicados del traspaso, y una partición de red larga no lo cancela ni pierde al jugador (la prueba corta la
  red 40 s simulados, cuatro veces el plazo de 10 s —`handoff_timeout_ticks`— tras el cual la 0.4 se rendía; el diseño
  no tiene límite de tiempo);
- una promesa («lo adopté», «te lo entrego») solo sale de un shard **después** de estar en su disco; si el disco falla,
  el shard deja de prometer (no miente);
- un traspaso repetido, retrasado o de una época vieja no resucita una cuenta ni la hace retroceder;
- el directorio y los shards coinciden al final, y si se pierde la carpeta del directorio se reconstruye a partir de los
  shards (`tsofgw store rebuild-directory`);
- un cierre limpio (Ctrl-C, SIGTERM) deja el mundo exactamente como estaba.

**Lo que se pierde en una caída** (no en un cierre limpio): como mucho el último intervalo de movimiento (los
jugadores se guardan cada `--player-save-ticks`, 100 ticks = 5 s por defecto; los NPC que deambulan, cada
`--npc-save-ticks`, 200 ticks = 10 s) y, claro, las conexiones en curso: los clientes tardan el tiempo de espera del
transporte (5 s) en notar que el servidor cayó y **no se reconectan solos** (con un cierre limpio se les avisa y lo
notan al instante). Lo que nunca se pierde es quién es dueño de qué.

**Lo que no hace:** no cifra nada; no tiene alta disponibilidad (el directorio es un solo proceso); no detecta que le
devuelvan una copia vieja de la carpeta; no replica los datos a otra máquina. Véanse los límites en la sección 11.

## 2. El almacén (`tsofgw-store`)

Un mapa clave-valor ordenado en memoria con un **diario** (*write-ahead log*) y **checkpoints**, hecho solo con la
biblioteca estándar y el HMAC del proyecto. Cada cambio se anota en el diario antes de aplicarse; de vez en cuando un
checkpoint guarda el mapa entero y empieza un diario nuevo; al arrancar se carga el último checkpoint y se repite el
diario.

- **Un lote es atómico:** varias operaciones se escriben como un solo registro. «Quitar el NPC de aquí y anotar que está
  en traspaso» ocurre entero o no ocurre.
- **Integridad con clave:** cada registro y cada checkpoint llevan un HMAC-SHA256. La secuencia (consecutiva) y la
  generación entran en la etiqueta, así que no se pueden quitar, repetir ni mezclar registros. Un daño del disco o una
  edición a mano se detecta; sin la clave no se puede fabricar un registro válido.
- **Caídas:** tras una caída el final del diario puede estar a medias. Se descarta (es lo esperado) y se informa
  (`torn_bytes`). Un registro dañado **fuera** de la cola es otra cosa: es un error, salvo con `--repair`, que corta el
  diario en el primer daño y sigue (y lo dice en voz alta, porque se pierde lo que venía después).
- **Qué es «la cola»:** el almacén sincroniza al menos cada 4 MiB, así que todo lo anterior a los últimos 8 MiB del
  diario seguro estaba en disco. Un daño dentro de esa ventana es indistinguible de una escritura a medias y se trata
  como tal: **alterar los últimos 8 MiB del diario se ve como una caída normal** (un `torn_bytes` mayor que cero sin que
  hubiera caída es la señal para investigar).
- **Ancla:** un almacén nuevo escribe de inmediato un checkpoint vacío autenticado. Abrir con la clave equivocada falla
  siempre con un error claro y **nunca borra nada**.
- **Checkpoint atómico:** se escribe a un archivo temporal, se sincroniza, se renombra y (en unix) se sincroniza la
  carpeta; tras una caída vale entera la generación vieja o entera la nueva. En Windows la biblioteca estándar no puede
  sincronizar una carpeta: se confía en que NTFS registre el renombrado en su propio diario.
- **Un proceso por carpeta:** un archivo `LOCK` con bloqueo exclusivo impide que dos procesos escriban a la vez.
- **Dos medios:** `FsMedium` (archivos: `snap-<generación>.bin` y `wal-<generación>.log`) y `MemMedium` (memoria con
  **inyección de fallos**: caídas con cola rota o con huecos de ceros, errores de escritura y de sincronización, caídas
  a mitad de un checkpoint). La lógica es la misma para los dos, así que se prueba una vez contra el medio que sabe
  fallar, en cada byte.

La clave de integridad se deriva (`derive_key`) de un secreto del operador y de una etiqueta: la **clave de clúster**
(con la etiqueta `shard-N` para el shard N, y `directory` para el directorio) o la **clave maestra** en un servidor suelto.
Cada carpeta tiene la suya, de modo que la de un shard no se puede pasar por la de otro. **Cambiar la clave deja la
carpeta ilegible** (`no se autentica con esta clave`); no hay rotación de claves todavía.

## 3. Qué guarda un shard

| Clave | Contenido |
|---|---|
| `m/init` | identidad del mundo y del shard (semilla, número de NPC, rejilla y número de shard): una carpeta no se mezcla con otro mundo ni con otro shard |
| `m/xfer`, `m/tick` | reserva de números de traspaso (para no repetirlos tras un reinicio) y el último reloj de ticks |
| `n/<npc>` | un NPC que este shard simula: posición, dirección, giro pendiente, generador aleatorio |
| `xo/<npc>` | un NPC en traspaso hacia otro shard, sin acuse todavía |
| `xi/<shard><xfer>` | un traspaso de NPC ya adoptado (para reconocer repeticiones tras un reinicio) |
| `p/<cuenta>` | un jugador residente aquí: posición y **época** |
| `ho/<cuenta>` | un jugador en traspaso hacia otro shard, sin acuse todavía (etapa: *esperando al directorio* o *anotado*) |
| `t/<cuenta>` | **lápida**: la cuenta salió de aquí (época y destino); una repetición vieja no la resucita |

**Cada NPC y cada cuenta vive bajo exactamente una de esas claves a la vez** (`n`/`xo` y `p`/`ho`): el paso de una a otra
se escribe en un solo lote, y por eso tras cualquier caída hay un dueño, nunca cero ni dos.

### Cuándo se escribe cada cosa

- **Lo crítico se sincroniza antes de decir nada a nadie.** Una promesa al otro lado («lo adopté», «te lo mando») se
  guarda en una cola y solo sale **después** del `commit` al final de la vuelta de `update` (`Persist::defer`). Un
  `commit` por vuelta, no por mensaje, agrupa todo lo del tick en una sola sincronización.
- **Lo no crítico se escribe sin sincronizar** y se hace durable cada `--sync-ticks` (20 por defecto): la posición de un
  jugador que camina (cada `--player-save-ticks`, y solo si se movió; escalonado por cuenta para no concentrar las
  escrituras) y el estado de los NPC (cada `--npc-save-ticks`, por trozos a lo largo del ciclo). Tras una caída se pierde
  como mucho ese intervalo de movimiento, nunca la propiedad de nada.
- **Compactación:** cuando el diario pasa de `--compact-mb` (16 MiB por defecto) se hace un checkpoint. **Un checkpoint
  bloquea el tick** mientras se escribe; con un mundo grande se nota (véase los límites).
- **Si el disco falla**, el almacén queda inutilizable hasta reabrirse (lo escrito podría estar o no) y el shard **deja de
  prometer**: descarta lo que tenía en cola y no envía ninguna promesa nueva (ni acuses ni traspasos); los buzones de
  salida de los demás reintentan, y se retoman solos cuando el shard se reinicia. Hay que reiniciarlo; `serve` lo dice por
  la salida de error.

## 4. El traspaso de un jugador, con disco y directorio

Es el traspaso del documento 09 con tres añadidos: el origen anota en disco antes de entregar, el directorio decide el
cambio de dueño, y el destino escribe antes de acusar. Cada cuenta lleva una **época** (1 al crearse; +1 en cada
traspaso) que ordena todo lo demás.

```
 origen                       directorio                      destino
   │  (el cliente ya está en el destino; se CONGELA al jugador)
   │ [disco, sincroniza] borra p/A, escribe ho/A «esperando al directorio»
   │── DirTransfer(A, época e, → destino) ──►│
   │                                         │ [disco, sincroniza] A → (destino, e+1)   ← el cambio de dueño
   │◄───────────── DirResult: Committed ─────│                                            ocurre aquí
   │ [disco, sincroniza] ho/A «anotado»
   │── HANDOVER(estado, época e+1) ─────────────────────────────────►│
   │                                                                  │ [disco, sincroniza] escribe p/A (e+1)
   │◄─────────────────────────────────────────────────── HAND_ACK ───│ adopta al jugador
   │ [disco] borra ho/A, escribe t/A (lápida: e+1, destino)
```

Sin directorio, los pasos del directorio no existen y el traspaso empieza ya en la etapa «anotado».

- **El cambio de dueño es la anotación del directorio**, que es un *compare-and-swap* sobre la época: «la cuenta está en
  mí con la época `e` y pasa a `destino`». Solo si el directorio tiene exactamente eso anota `(destino, e+1)`. Repetir la
  misma petición vuelve a responder `Committed` sin cambiar nada (es idempotente). Cualquier otra cosa se rechaza con lo
  que el directorio tiene de verdad. **El origen no envía el estado al destino hasta recibir el `Committed`**, así que dos
  shards no pueden quedarse con la cuenta.
- **El buzón de salida (`ho/`) no se rinde nunca** con persistencia: sin acuse, reintenta (despacio, cada
  `slow_retry_ticks`, tras el plazo normal). Es independiente de la sesión: sigue vivo aunque el cliente se desconecte, y
  con persistencia también está en disco, así que sobrevive a un reinicio del origen.
- **El destino mira la época antes de adoptar:** una época que ya conoce (la suya actual, la de una lápida o la de un
  buzón de salida) se **acusa sin adoptar** (`handovers_stale`): un `HANDOVER` repetido o tardío no resucita ni hace
  retroceder a nadie. Si la cuenta ya está conectada ahí con otra época, se rechaza (`handover_conflicts`: no debería
  ocurrir). Si el propio destino está entregando esa cuenta, lo ignora y el origen reenvía (`handovers_deferred`).
- **El destino escribe `p/A` antes de acusar** y el origen borra `ho/A` y deja la lápida solo al recibir el acuse.

### Qué pasa si algo se cae

| Se cae... | ...en este punto | Al volver |
|---|---|---|
| el origen | antes de anotar `ho/A` | la cuenta sigue residente en el origen; el traspaso se reintenta |
| el origen | con `ho/A` «esperando al directorio» | restaura `ho/A` y repite el `DirTransfer` (idempotente) |
| el directorio | antes de anotar | nada cambió; el origen repite la petición y se anota entonces |
| el directorio | tras anotar, antes de responder | el origen repite; responde `Committed` (repetido) |
| el origen | con `ho/A` «anotado» | restaura `ho/A` y reenvía el estado hasta el acuse, sin límite |
| el destino | tras recibir el estado, antes de escribir `p/A` | no hubo acuse: el origen reenvía y el destino adopta entonces |
| el destino | tras escribir `p/A`, antes de acusar | la cuenta ya es suya; el reenvío llega con una época conocida y se acusa sin adoptar |
| el origen | tras el acuse, antes de la lápida | restaura `ho/A`, reenvía; el destino acusa como repetido |
| la red | una partición larga (la prueba: 40 s simulados) | el origen reintenta despacio, sin rendirse; al volver, se completa |

Esta tabla es la explicación, no la prueba. Las pruebas fuerzan la caída en tres momentos (antes de que el directorio anote
—que solo existe con directorio—, antes de que el destino adopte y antes de que llegue el acuse), cayendo el origen, el
destino, los dos o el directorio, con un corte limpio y con una cola rota de 5 bytes: 36 combinaciones entre con y sin
directorio (sección 9).

### Sin directorio

Se puede usar persistencia sin directorio (`--data-dir` sin `--directory`). Entonces cada shard decide por lo que
tiene, y **no se garantiza la unicidad de una cuenta ante cualquier fallo**: es el mejor esfuerzo posible con (1) la
lápida como «puntero de reenvío» (un cliente que entra por un shard que ya no tiene la cuenta es llevado a donde fue,
y si ese también la entregó, más lejos) y (2) la regla de que **una cuenta nueva solo se crea en su shard de origen**
(el que posee su punto de aparición), de modo que un shard que no la conoce no la inventa mientras la de verdad viaja
en un traspaso. Para un MMO real, usa el directorio.

## 5. El directorio de sesiones

Un proceso (`tsofgw directory`) con su propio almacén. Guarda `(shard, época)` por cuenta y responde por el bus entre
shards (autenticado con la clave de clúster, como todo lo del bus):

| Mensaje | Quién → quién | Para qué |
|---|---|---|
| `DirLogin(cuenta, origen)` | shard → directorio | «esta cuenta quiere entrar aquí»; si el directorio no la conoce, la anota en `origen` (el dueño de su punto de aparición) con la época 1 |
| `DirRoute(cuenta, shard, época)` | directorio → shard | «la cuenta vive en `shard` desde la época `época`» |
| `DirTransfer(cuenta, época, destino)` | shard → directorio | el *compare-and-swap* del traspaso |
| `DirResult(estado, shard, época)` | directorio → shard | `Committed`, `Rejected` (otro dueño u otra época) o `Unknown`, con lo que el directorio tiene ahora |

- **Escribe antes de responder.** Una respuesta que llegó a un shard sigue siendo cierta aunque el directorio caiga y
  rearranque justo después. Si su disco falla, **descarta todas las respuestas** (también las de lectura: todas pasan por la misma cola
  que espera al `commit`) y no vuelve a contestar hasta reiniciarse: nadie entra ni cruza bordes con una cuenta mientras
  tanto; lo que ya está jugando sigue jugando.
- **Es un solo proceso.** Sin réplica, sin elección de líder, sin arrendamientos, sin sustitución automática. Si cae,
  nadie puede entrar ni cruzar bordes hasta que vuelva (los traspasos esperan, sin perder a nadie; las pruebas lo
  comprueban).
- **Si se pierde su carpeta**, se reconstruye a partir de los shards con el comando `store rebuild-directory`, **con todos
  los shards parados**: cada uno sabe qué cuentas tiene y cuáles está entregando. Si dos shards reclaman la misma cuenta
  con la misma época, la reconstrucción se niega a elegir por su cuenta y las lista (`--accept-conflicts` las anota en el
  shard donde están residentes, o en el de menor número si son varios, y hay que revisarlas a mano). No reconstruye
  encima de una carpeta con datos.
- **Todos deben compartir la configuración del mundo:** el bus descarta lo que viene de un proceso con otra configuración,
  y la huella incluye si hay directorio. Por eso `tsofgw directory` acepta `--npcs`, `--radius`, `--sim-radius`,
  `--tick-ms` y `--seed`, y deben coincidir con los de los shards.

## 6. Entrar en el juego

Qué hace un shard cuando un cliente pide entrar (`JOIN`) con persistencia:

| Situación | Con directorio | Sin directorio |
|---|---|---|
| un shard le está traspasando esa cuenta | el jugador llega con el estado del traspaso (`Arriving`) | igual |
| este shard acaba de entregarla y espera el acuse | espera: no entra ni aquí ni en el destino | igual |
| tiene su registro `p/` | entra con su posición restaurada | igual |
| no lo tiene y hay lápida hacia otro shard | (lo decide el directorio) | redirige al cliente al shard de la lápida |
| no lo tiene y no es su shard de origen | pregunta al directorio y redirige a donde diga | redirige al shard de origen |
| cuenta nueva y es su shard de origen | el directorio la crea (época 1) y entra en su punto de aparición | la crea aquí (época 1) |
| el directorio dice que es de este shard pero el estado aún no llegó | espera (viene en un traspaso en curso; el origen reenvía) | — |
| el directorio no contesta | espera hasta `login_timeout_ticks` (600 = 30 s) y entonces expulsa la sesión; quien conecte puede volver a intentarlo (los clientes de los bancos de pruebas lo hacen solos) | — |

Un cliente con conector (los bots con `--shards` lo tienen) sigue las redirecciones de entrada solo
(`login_redirects` en sus estadísticas), así que puede conectarse a cualquier shard. Las posiciones de aparición dependen de la semilla y de la cuenta, así que cualquiera puede calcular de antemano el
shard de origen de una cuenta nueva.

## 7. Cierre limpio y caídas

- **Cierre limpio** (`Ctrl-C`, `SIGINT`, `SIGTERM`; en Windows `Ctrl-C` y `Ctrl-Break`): guarda los NPC y los jugadores
  conectados, sincroniza, compacta y **avisa a los clientes** (reciben `DISCONNECT` al instante en lugar de esperar el
  tiempo de espera). Lo hace `tsofgw serve` y `tsofgw directory`. El estado queda tal cual estaba: una prueba lo comprueba
  contra el mundo exacto de antes del cierre.
- **Caída** (`kill -9`, corte de luz): al arrancar se carga el último checkpoint, se repite el diario y se descarta la cola
  rota; `serve` imprime qué restauró (NPC, traspasos pendientes, residentes, lápidas, generación, registros repetidos y
  bytes descartados). Los traspasos pendientes se retoman solos.
- **Un shard que arranca con una carpeta vacía** inicia el mundo desde la semilla; con una carpeta con datos la restaura;
  con una carpeta de otro mundo, de otro shard o con otra clave, **se niega a arrancar** sin tocar nada.

## 8. Cómo se pone en marcha

```
tsofgw keygen -o master.key
tsofgw keygen -o cluster.key

# Un servidor suelto con disco (la clave de integridad se deriva de la clave maestra)
tsofgw serve --master-file master.key --bind 0.0.0.0:7000 --npcs 20000 --data-dir datos

# Un cluster de 4 shards con directorio. Cada shard con su carpeta; todos con la misma rejilla, lista, clave y mundo.
tsofgw directory --grid 2x2 --cluster-key-file cluster.key --bind 127.0.0.1:7200 --data-dir datos-dir \
                 --peers 127.0.0.1:7100,127.0.0.1:7101,127.0.0.1:7102,127.0.0.1:7103 --npcs 20000
tsofgw serve --master-file master.key --cluster-key-file cluster.key --shard 0 --grid 2x2 --bind 0.0.0.0:7000 \
             --peers 127.0.0.1:7100,127.0.0.1:7101,127.0.0.1:7102,127.0.0.1:7103 --npcs 20000 \
             --data-dir datos0 --directory 127.0.0.1:7200
tsofgw serve ... --shard 1 --data-dir datos1 --directory 127.0.0.1:7200      # y así hasta el 3

# Qué hay guardado (con el proceso parado: la carpeta está bloqueada mientras corre)
tsofgw store info --data-dir datos0 --cluster-key-file cluster.key --shard 0
tsofgw store info --data-dir datos-dir --cluster-key-file cluster.key --directory
tsofgw store info --data-dir datos --master-file master.key --standalone

# Perdida la carpeta del directorio: reconstruirla desde los shards (todos parados)
tsofgw store rebuild-directory --shard-dirs datos0,datos1,datos2,datos3 --data-dir datos-dir --cluster-key-file cluster.key

# Una tormenta de caídas de shards y del directorio, en un solo proceso y con tiempo simulado
tsofgw loadtest --shards 2x2 --clients 100 --npcs 4000 --seconds 20 --directory --chaos 12
```

Opciones de `serve`: `--data-dir CARPETA` (activa la persistencia), `--sync-ticks`, `--npc-save-ticks`,
`--player-save-ticks`, `--compact-mb`, `--repair` (tolerar daño en medio del diario descartando lo posterior; por defecto
se niega), y `--directory DIR` (la dirección del directorio, solo en un shard con `--data-dir`). **Las claves nunca van por la
línea de comandos**, solo por archivo, como siempre. `store info` abre la carpeta como lo haría el servidor al arrancar
(descarta una cola rota) pero **no crea nunca un almacén**: apuntado a una carpeta vacía o equivocada, dice que no hay nada.

`serve` imprime cada 5 s, además de lo de siempre, una línea de **sesiones** (entradas redirigidas, esperando al
directorio, traspasos sin acuse) y una de **almacén** (commits, NPC y jugadores guardados, compactaciones, tamaño del
diario, promesas retenidas). El directorio imprime sus cuentas por shard y los cambios de dueño confirmados, repetidos,
rechazados y desconocidos.

**Copias de seguridad:** copia la carpeta con el proceso parado (un archivo de checkpoint y un diario copiados en momentos
distintos pueden no encajar). Restaurar una copia vieja **no se detecta** y, en un cluster, es peligroso: las cuentas que
el directorio sitúa en ese shard desde la copia ya no tienen su estado allí (esperan un traspaso que no llegará y no
pueden entrar), y quedan copias viejas de las que se fueron. Restaura siempre todo el cluster a la vez.

## 9. Cómo se probó

**Suite en proceso** (`crates/tsofgw-online/tests/persist.rs`, 15 pruebas, con el cluster completo en marcha y bots cruzando
bordes, sobre una red simulada con latencia y jitter y almacenes en memoria con fallos inyectables). Se exige, además de
lo de siempre (ningún NPC ni jugador perdido o duplicado), que **durante toda la marcha** (cada 100 ms simulados) ninguna
cuenta esté residente en dos shards, ninguna esté simulada por dos shards a la vez, ninguna desaparezca, el directorio no
sitúe a una cuenta en otro shard que el que la tiene y ningún NPC se quede sin dueño. Cubre: un cluster con almacén se
comporta como sin él; un shard que cae y vuelve; una tormenta de caídas aleatorias; un NPC a medias de un traspaso;
**una caída en cada punto del traspaso de un jugador** (con y sin directorio, en el origen, el destino, los dos y el
directorio, con corte limpio y con cola rota); particiones largas; directorio caído; una cuenta que entra dos veces a la
vez; directorio perdido y reconstruido; directorio que no conoce la cuenta; un disco que falla justo al prometer; cierre
limpio exacto; pérdida acotada en una caída; y traspasos viejos, repetidos o con época falsificada inyectados a mano.

**La tormenta, desde la línea de comandos** (`tsofgw loadtest --shards 2x2 --directory --chaos N`): cada ronda tumba a un
shard (a veces a dos a la vez, a veces al directorio) en un momento aleatorio, con cola rota o limpia, lo reinicia, y los
clientes se reconectan solos. Falla si en algún momento se vio una violación o si al final algo no cuadra. Ejemplo
(la orden de la integración continua: 2×2, 32 clientes, 8 rondas, semilla 11): 9 caídas de shard (3 dobles) y 2 del directorio, 57 reconexiones, y al final
`cuentas: 32 con registro | duplicadas 0 | perdidas 0 | distintas del directorio 0`.

**Procesos reales** (`crates/tsofgw-cli/tests/cli.rs`): servidor con carpeta de datos que se restaura y rechaza una clave
ajena y un mundo ajeno; SIGTERM que cierra guardando (unix); y un cluster de **2 shards y un directorio como procesos
separados** con 10 bots que cruzan el borde: se les hace `kill -9` al shard 1 y al directorio, se reinician con sus
carpetas, los mismos jugadores vuelven a entrar (redirigidos por el directorio restaurado) y siguen cruzando; al
terminar, cada cuenta está en un solo shard, el directorio coincide con los shards, no queda ningún traspaso sin acuse
y `store rebuild-directory` reconstruye el mismo reparto.

**Prueba de las pruebas** (`python ci/mutaciones.py`, véase su cabecera): estropea el código a propósito, de uno en uno,
con 23 defectos realistas (9 en el almacén y 14 en la persistencia del cluster: acusar antes de escribir, responder desde
un directorio con el disco caído, no comprobar una etiqueta, olvidar la lápida, aceptar una época vieja...) y exige que la
suite falle con cada uno. Un defecto que nadie nota es un hueco en la suite. **Encontró uno** (S7: un checkpoint que no
sincronizara antes el diario viejo no lo notaba ninguna prueba del almacén), que se cerró con una prueba nueva.
Resultado de la ejecución completa, con el código final de la 0.5.0: **23 de 23 defectos detectados**.

**Coste del disco real** (procesos `serve` y `bots` reales, 4000 NPC, 60 bots, 2 hilos, en una máquina virtual de 2 núcleos
con un disco virtual rápido: una escritura de 200 B con `fsync` tarda de mediana 0,14 ms, p99 0,5 ms, máximo 20 ms en 3000
muestras). En 90 s el tick medio fue de **1,46 ms con `--data-dir` y 1,45 ms sin él** (máximos de 9,4 y 6,2 ms); en tres
ejecuciones de 22 s, de 1,28 a 1,39 ms con persistencia y de 1,20 a 1,33 ms sin ella. Con persistencia, en esos 90 s hubo
89 sincronizaciones (una por segundo: `--sync-ticks` 20), 11 193 NPC y 1064 jugadores guardados y un diario de 564 KB. Son
cifras del orden del ruido de la máquina. **Una de las cinco ejecuciones con persistencia tuvo un tick de 135 ms**
(ninguna sin ella pasó de 6 ms); no se repitió en las otras cuatro y no sé si fue el disco virtual o el entorno; es
coherente con lo que se espera de un `fsync` en el hilo del tick. Un disco más lento (disco duro, volumen de red) dará
cifras peores y habría que medirlo.

**Integración continua:** en Linux, Windows y macOS corren las pruebas (incluidas las de procesos reales; solo la de
`SIGTERM` es exclusiva de unix) y la tormenta de caídas con directorio. En Linux, además, se comprueba en cada ejecución
que cada parche del catálogo de la prueba de las pruebas aún se aplica. La prueba de las pruebas completa se lanza a mano
desde la pestaña *Actions* («Run workflow»): **ese trabajo manual no se ha ejecutado nunca en Actions** (sí en local, con
el resultado de arriba). Lo que la CI ha confirmado de verdad está en el documento 07.

## 10. Defectos encontrados al construirlo

- **Un hueco en las pruebas del almacén** (S7, arriba): lo encontró la prueba de las pruebas, no la revisión.
- **Los clientes seguían creyendo que jugaban tras un cierre limpio** hasta que vencía el tiempo de espera del transporte
  (5 s). Una prueba que reinicia todos los shards y espera que todos los clientes juegan lo mostró (23 de 24). Ahora el
  cierre expulsa a los clientes con aviso.
- **`store info` creaba un almacén vacío** si se le daba una ruta equivocada (abrir una carpeta vacía crea uno). Lo
  encontró al escribir las pruebas de procesos reales; ahora comprueba que existe antes de abrir.
- **El banco de pruebas se equivocaba, no el motor:** contaba como «época mínima» registros de un shard cuyo disco había
  fallado (no durables) y daba falsas pérdidas; y comprobaba el reparto exacto en un instante en que un NPC cruzaba un
  borde (un duplicado transitorio legítimo). Ahora ignora los shards con el disco fallado y espera a que el reparto se
  asiente.
- **Un jugador que se mueve 400 ms al máximo de velocidad** recorre más que mi primera cota de «un traspaso viejo no hace
  retroceder a nadie»: la cota era demasiado estricta, no el motor.

## 11. Límites conocidos

- **No hay cifrado**: ni de la red (jugadores y bus) ni de lo que se guarda en disco. **Quien tenga la clave de clúster
  puede hacerse pasar por cualquier shard y por el directorio**; es confianza total.
- **El directorio es un solo proceso**, sin alta disponibilidad, sin arrendamientos y sin sustitución automática. Mientras
  está caído nadie entra ni cruza bordes con una cuenta (lo que ya juega sigue jugando). Se reinicia a mano y se reconstruye
  a mano si se pierde su carpeta.
- **No se detecta el retroceso:** restaurar una copia vieja de una carpeta (o devolverle sus archivos de ayer) no se nota:
  la clave autentica el contenido, no la frescura. Hace falta un ancla fuera de esa máquina.
- **Alterar los últimos 8 MiB del diario** se toma por una escritura a medias y se descarta sin error (`torn_bytes`).
- **Las lápidas crecen** (una por cuenta que alguna vez salió de un shard). Hay que podarlas cuando se pueda demostrar que
  nada en vuelo las necesita; hoy no se podan.
- **El almacén vive en la memoria** del proceso (un mapa ordenado de todo lo guardado) y **un checkpoint bloquea el tick**
  mientras se escribe: para mundos muy grandes habría que hacerlo incremental o en otro hilo.
- **La ventana de repeticiones de NPC** (`xfer_memory_ticks` = 1200) es finita: un acuse que tardara más que eso en llegar
  podría adoptar un NPC dos veces. Con persistencia el buzón de salida de NPC tampoco se rinde, pero la memoria de adoptados
  sí caduca.
- **Sin directorio, la unicidad de una cuenta no está garantizada** ante cualquier fallo (véase la sección 4).
- **Un shard con el disco fallado deja de prometer** y hay que reiniciarlo; no hay recuperación en caliente.
- **Los clientes no se reconectan solos** tras una caída (sí en los bancos de pruebas): notan la caída al vencer el tiempo
  de espera del transporte. Un cierre limpio los avisa al instante.
- **El manejador de señales de Windows** (`Ctrl-C`/`Ctrl-Break`) solo se comprobó con el compilador (tipos), no en una
  ejecución real; en unix se prueba con una señal de verdad.
- **El coste del disco no está medido en un disco real de producción.** El simulador mide los traspasos con almacenes en
  memoria, sin el coste de `fsync`: con `loadtest --shards 2x2 --clients 200 --npcs 4000 --seconds 20 --patrol 100` (unos
  235 traspasos por ejecución, bus simulado de 10 ms de latencia) un traspaso tarda de media 37 ms sin persistencia, 43 ms
  con ella (lo prometido sale al final de la vuelta del tick, después del `commit`) y 67 ms con directorio (una ida y vuelta
  más); los máximos son 200, 400 y 250 ms. La resolución es de un tick (50 ms), así que son órdenes de magnitud. Con disco
  real cada traspaso fuerza una sincronización en el origen (dos con directorio), una en el destino y una en el
  directorio, y el coste lo marca el disco; lo que se midió en el de este entorno está en la sección 9 y **no sirve para
  predecir un disco lento**. Además, **el almacén escribe y sincroniza en el hilo del tick**: un `fsync` lento se nota
  como un tick largo.
- **Todo se probó en una sola máquina** (bucle local, 2 núcleos), no en varias ni con latencias de internet.
