# Cambios

## 0.5.0 — Persistencia y directorio de sesiones
Responde a lo que más dolía del clúster de la 0.4: un shard reiniciado perdía su estado, un traspaso perdido durante una partición larga podía perder a un jugador, y nada impedía que una cuenta estuviera en dos shards a la vez. Se empezó **después de leer la CI de la 0.4.3** (en verde), así que lo nuevo se apoya en algo confirmado fuera de mi máquina; **lo nuevo de la 0.5.0 todavía no ha corrido en la CI**.

**Resultado de la ejecución de la 0.4.3** (commit `abe28d2`; los 7 trabajos en verde)
- Linux, Windows y macOS con el compilador fijado (1.97.0): formato, Clippy sin avisos y las **228 pruebas**; los recuentos de las pruebas de humo coinciden (clúster simulado de 2 shards: 6 traspasos de jugador, 0 pérdidas ni duplicados). Sobre UDP real (solo Linux): 8 traspasos, 0 perdidos o duplicados. En Windows y macOS esa prueba de humo se salta a propósito.
- **Rust estable más reciente (1.100.0, informativo): en verde**, con formato, Clippy y las 228 pruebas: los 9 avisos que quedaban estaban corregidos.
- **wgpu** con llvmpipe: la prueba pasa y la escena `mixta` a 320×240 da una diferencia media de 0,051/255 y máxima de 1.

**Un almacén durable nuevo: `tsofgw-store`**
- Mapa clave-valor ordenado con **diario** (*write-ahead log*) y **checkpoints**, solo con la biblioteca estándar y el HMAC del proyecto. Lotes atómicos; cada registro y cada checkpoint llevan un HMAC-SHA256 en el que entran la secuencia y la generación (no se pueden quitar, repetir ni mezclar registros); una escritura a medias al final del diario se descarta y se informa (`torn_bytes`), un daño en medio es un error (o, con `--repair`, se corta ahí y se avisa); abrir con la clave equivocada falla y **nunca borra nada**; un almacén nuevo escribe un checkpoint ancla autenticado.
- Dos medios: archivos reales (`snap-<generación>.bin`, `wal-<generación>.log`, bloqueo `LOCK` exclusivo, checkpoint con archivo temporal, sincronización y renombrado) y memoria **con fallos inyectables** (caída limpia, con cola rota o con huecos, errores de escritura y de sincronización, caída antes o después de renombrar un checkpoint). La misma lógica se prueba en cada byte contra el medio que sabe fallar.
- Límite honesto: el almacén sincroniza al menos cada 4 MiB, así que lo anterior a los últimos 8 MiB del diario seguro estaba en disco; un daño dentro de esa ventana es indistinguible de una escritura a medias y se descarta.

**El clúster guarda su estado**
- Cada shard guarda sus NPC (posición, dirección, giro, generador), sus jugadores (posición y **época**), sus traspasos pendientes (de NPC y de jugador) y **lápidas** (una cuenta que salió de aquí no resucita por una repetición vieja). Cada NPC y cada cuenta está bajo exactamente una clave a la vez: el paso de una a otra es un solo lote atómico, y por eso tras cualquier caída hay un dueño, nunca cero ni dos.
- **Escribir antes de prometer:** una promesa al otro lado («lo adopté», «te lo entrego») sale solo después del `commit` al final de la vuelta del tick. Si el disco falla, el shard descarta lo retenido y deja de prometer; no miente.
- Lo no crítico (la posición de quien camina, el estado de los NPC) se escribe sin sincronizar y se hace durable cada pocos ticks: una caída pierde como mucho ese intervalo de movimiento, nunca la propiedad de nada. Un cierre limpio lo guarda todo.
- Los traspasos pendientes **se retoman tras un reinicio** y, con persistencia, el buzón de salida **no se rinde nunca** (reintenta despacio): una partición larga ya no puede perder a un jugador (la prueba corta la red 40 s simulados, cuatro veces el plazo de 10 s tras el cual la 0.4 se rendía).

**Directorio de sesiones**
- Un proceso nuevo (`tsofgw directory`) con su propio almacén guarda `(shard, época)` por cuenta. Cuatro mensajes nuevos en el bus: `DirLogin`, `DirRoute`, `DirTransfer` y `DirResult`. El cambio de dueño es un *compare-and-swap* sobre la época, idempotente, **anotado en disco antes de responder**; el origen no envía el estado hasta tener la anotación y el destino no acusa hasta haberlo escrito. `Handover` lleva ahora la época: un traspaso repetido, tardío o con una época que el destino ya conoce no resucita ni hace retroceder a nadie.
- **Entrar:** con directorio, un shard que no es el dueño redirige al cliente; el cliente con `Connector` sigue la redirección solo. Las cuentas nuevas se crean en el shard que posee su punto de aparición, y sin directorio también (mejor esfuerzo con lápidas como punteros de reenvío; la unicidad solo se garantiza con directorio).
- **Si se pierde su carpeta,** `tsofgw store rebuild-directory` la reconstruye a partir de lo que guardan los shards (parados); se niega a elegir por su cuenta entre dos reclamaciones iguales (`--accept-conflicts`) y a reconstruir sobre una carpeta con datos.
- **Incompatible con la 0.4.x en el bus:** `Handover` cambió de formato y la huella de configuración ahora incluye si hay directorio (versión 2), así que los shards 0.4.x y 0.5.0 se rechazan entre sí como «otra configuración de mundo». Actualiza todo el clúster a la vez.

**Línea de comandos**
- `tsofgw serve --data-dir CARPETA` (más `--sync-ticks`, `--npc-save-ticks`, `--player-save-ticks`, `--compact-mb`, `--repair`, y `--directory DIR` en un shard); `tsofgw directory …`; `tsofgw store info` (con `--shard`, `--directory` o `--standalone`; **no crea nunca un almacén**) y `tsofgw store rebuild-directory`; `tsofgw loadtest --directory --chaos N [--chaos-seed S]`. Las claves siguen sin aceptarse por la línea de comandos.
- **Cierre limpio:** `Ctrl-C`, `SIGINT` y `SIGTERM` (en Windows, `Ctrl-C` y `Ctrl-Break`; `tsofgw-sys::stop`) guardan el estado, compactan y **expulsan a los clientes con aviso**. `tsofgw bots` imprime ahora `cambios de shard N (F fallidos) | entradas redirigidas E`.

**La prueba de las pruebas (`ci/mutaciones.py`)**
- Estropea el código a propósito, de uno en uno, con 23 defectos que un programador podría cometer de verdad (9 en el almacén y 14 en la persistencia del clúster: acusar antes de escribir, responder desde un directorio con el disco caído, no comprobar una etiqueta, olvidar la lápida, aceptar una época vieja, rendirse tras un tiempo…) y exige que la suite falle con cada uno. Resultado de la ejecución completa: **23 de 23 defectos detectados** (14 de la persistencia del clúster y 9 del almacén), ejecutados con el código final de esta versión.
- **Encontró un hueco real** (S7): un checkpoint que no sincronizara antes lo escrito sin sincronizar lo perdía en una caída sin que ninguna prueba del almacén lo notara. Se cerró con una prueba nueva (`un_checkpoint_que_no_llega_a_instalarse_no_pierde_lo_escrito_sin_sincronizar`).
- La CI comprueba en cada ejecución (Linux) que cada parche del catálogo aún se aplica, y hay un trabajo manual (*Run workflow*) para la ejecución completa. **Ese trabajo manual no se ha ejecutado nunca en Actions.**

**Cómo se probó**
- **292 pruebas pasan**, con la librería C y con el respaldo en Rust puro (la 0.4.3 tenía 228). Entre ellas, 15 pruebas de persistencia del clúster que vigilan **durante toda la marcha** (cada 100 ms simulados) que ninguna cuenta esté en dos shards, ninguna desaparezca y ningún NPC se quede sin dueño: una tormenta de caídas aleatorias, **una caída en cada punto del traspaso de un jugador** (origen, destino, los dos y el directorio; corte limpio y cola rota; con y sin directorio), particiones largas, directorio caído y reconstruido, un disco que falla justo al prometer, y traspasos viejos, repetidos o con época falsificada.
- Desde la línea de comandos: `loadtest --shards 2x2 --directory --chaos 8` (la orden de la CI): en 8 rondas, 9 caídas de shard (3 dobles) y 2 del directorio, 57 reconexiones de clientes y, al final, 32 cuentas con 0 duplicadas, 0 perdidas y 0 distintas del directorio; en lo vigilado cada 100 ms, 0 violaciones.
- **Procesos reales:** un cluster de 2 shards y un directorio como procesos separados, con 10 bots que cruzan el borde; se hace `kill -9` a un shard y al directorio, se reinician con sus carpetas, los mismos jugadores vuelven a entrar y al final cada cuenta está en un solo shard, el directorio coincide con los shards y `store rebuild-directory` reconstruye el mismo reparto. También: `SIGTERM` que cierra guardando (unix), y la negativa a abrir con otra clave o con la carpeta de otro mundo.

**Medidas (simulador, no un disco real)**
- Tiempo de un traspaso de jugador con `loadtest --shards 2x2 --clients 200 --npcs 4000 --seconds 20 --patrol 100` (unos 235 traspasos por ejecución, bus simulado de 10 ms): de media **37 ms** sin persistencia, **43 ms** con ella y **67 ms** con directorio (una ida y vuelta más); máximos de 200, 400 y 250 ms. La resolución es de un tick (50 ms): son órdenes de magnitud. El simulador no modela el coste de `fsync`: con disco real cada traspaso fuerza sincronizaciones en el origen, el destino y el directorio. **Coste en un disco real** (procesos `serve` y `bots`, 4000 NPC, 60 bots; disco virtual rápido, `fsync` de mediana 0,14 ms): en 90 s el tick medio fue de 1,46 ms con `--data-dir` y 1,45 ms sin él (máximos de 9,4 y 6,2 ms); una de las cinco ejecuciones con persistencia tuvo un tick de 135 ms que no se repitió. El almacén sincroniza en el hilo del tick: un `fsync` lento se nota como un tick largo. No dice nada de un disco duro o un volumen de red.

**Defectos encontrados al construirlo** (documento 10, sección 10)
- Tras un cierre limpio los clientes seguían creyendo que jugaban hasta que vencía el tiempo de espera del transporte (5 s): ahora se les avisa.
- `tsofgw store info` creaba un almacén vacío si se le daba una ruta equivocada: ahora comprueba que existe y no crea nada.
- El banco de pruebas contaba como pérdida lo que eran registros no durables de un shard con el disco caído, y comprobaba el reparto exacto en un instante en que un NPC cruzaba un borde: era el banco, no el motor.
- El hueco S7 del almacén, arriba.

**Límites (documento 10, sección 11)**
- **Sin cifrado** de la red ni del disco; **la clave de clúster es confianza total** (también suplanta al directorio); **el directorio es un solo proceso** sin alta disponibilidad; **no se detecta que devuelvan una copia vieja** de una carpeta; un daño en los últimos 8 MiB del diario se toma por una escritura a medias; las lápidas crecen; el almacén vive en memoria y un checkpoint bloquea el tick; la ventana de repeticiones de NPC es finita; un shard con el disco fallado deja de prometer hasta reiniciarse; los clientes no se reconectan solos tras una caída; el manejador de señales de Windows solo se comprobó con el compilador; todo se probó en una sola máquina.

## 0.4.3 — La CI en verde
Tercera lectura de la integración continua (la ejecución de la 0.4.2, commit `5849357`): **terminó en verde**. Esta versión no añade funciones ni cambia comportamiento: corrige los 9 avisos que el Rust estable más reciente (1.100.0) todavía marca en el trabajo informativo y deja registrados los resultados reales.

**Resultado de la ejecución de la 0.4.2**
- **Windows:** por primera vez corren sus pruebas con esta suite (`x86_64-pc-windows-msvc`): **228 pruebas pasan**, incluidas las de UDP real en el bucle local, y las pruebas de humo acaban sin pérdidas ni duplicados.
- **Mismos números en los tres sistemas:** los recuentos de paquetes, snapshots y traspasos de las pruebas de humo son idénticos en Linux, macOS y Windows (la simulación es determinista); solo cambian los tiempos. Clúster simulado: 6 traspasos de jugador completados, 590 mensajes entre shards, 0 jugadores y 0 NPC perdidos o duplicados. Sobre UDP real (Linux): 8 traspasos, 0 perdidos o duplicados.
- **wgpu:** compila contra wgpu 30.0.2 y la prueba que compara la GPU con el rasterizador por software (triángulo, cubo y mixta) pasa. Con el controlador por software llvmpipe, `gpu_render` da para la escena `mixta` a 320×240 una diferencia media de 0,051/255, 0 % de píxeles con diferencia > 16 y máxima de 1: **COINCIDEN**.
- También en verde: Linux y macOS (228 pruebas cada uno), el trabajo sin compilador de C y el compilador mínimo declarado (1.90).
- **Rust estable más reciente (informativo):** las pruebas pasan y Clippy marca 9 avisos. Cargo se detiene en el primer fallo y los 7 de la 0.4.1 los ocultaban; con `--keep-going` (0.4.2) salieron todos de una vez.

**Clippy 1.100 (9 avisos, sin cambio de comportamiento)**
- `chunks_exact_to_as_chunks` (8): `tsofgw-cli` (`render` y su prueba de extremo a extremo) y seis pruebas de `tsofgw-soft` usan `as_chunks`.
- `needless_range_loop` (1): el generador de snapshots de las pruebas de `replicate.rs` recorre `e.pos` con `iter_mut()`; mismos números aleatorios en el mismo orden.

## 0.4.2 — Lo que encontró la CI
Segunda lectura real de la integración continua (la ejecución de la 0.4.1, commit `ad315f2`). Esta versión no añade funciones: corrige lo que esa ejecución encontró, y deja para después —a propósito— todo lo nuevo hasta ver la CI en verde.

**Resultado de la ejecución de la 0.4.1**
- **En verde:** Linux y macOS con el compilador fijado (1.97.0): formato, Clippy sin avisos, **228 pruebas** y las pruebas de humo (cargas, clúster simulado y, en Linux, clúster sobre UDP real: 8 traspasos de jugador y 0 jugadores o NPC perdidos o duplicados); el trabajo sin compilador de C; y el compilador mínimo declarado (1.90).
- **El rojo de Clippy de la 0.4.0 era el compilador:** con el estable más reciente (1.100.0) Clippy marca 7 avisos nuevos; con el compilador fijado, ninguno. Las pruebas con el 1.100.0 pasan.
- **En rojo:** Windows (el trabajo murió a los 23 s, sin ejecutar pruebas) y wgpu (un error de tipos al compilar).

**CI**
- **Windows:** el paso «Versiones» ejecutaba `bash -c '…'` desde `ci/annotate.py`; en Windows `bash` resuelve a `C:\Windows\System32\bash.exe` (el lanzador de WSL), no a Git Bash, y fallaba con «no hay distribuciones instaladas». Ahora lo hace `ci/versiones.py`, que llama a `rustc` y `cargo` directamente. `annotate.py` usa el mismo Python que lo ejecuta y su ayuda avisa de no usar `bash`.
- Clippy corre con `--keep-going`: un error en un crate ya no oculta los avisos de los demás.
- `actions/checkout` y `actions/setup-python` pasan de v4/v5 a v7 (Node 24): GitHub avisaba de que las de Node 20 se están retirando.
- `.gitattributes` nuevo: el repositorio y las copias de trabajo usan LF en cualquier sistema (CRLF solo en `*.ps1`), para que Windows vea los mismos bytes que Linux y macOS.

**wgpu (`gpu/tsofgw-wgpu`)**
- `VertexState::buffers` es `&[Option<VertexBufferLayout>]` en wgpu 30.0.2: se envuelve en `Some(...)`. Es el error de compilación que encontró la CI.
- Se contrastaron con el **código fuente** de wgpu 30.0.2 todas las estructuras y métodos que usa el crate (descriptor de la tubería, pase de render y sus adjuntos, copia de textura a búfer y las firmas de `Device`, `Buffer`, `BufferSlice`, `Queue` y `RenderPass`), y la maqueta con la que se comprueban los tipos ahora refleja esas definiciones (antes no tenía el `Option`, y por eso la comprobación local no vio el error).

**Clippy 1.100 (7 avisos, sin cambio de comportamiento)**
- `chunks_exact_to_as_chunks` (5): `parse_input`, `SoftGraphics::clear`, `SoftGraphics::draw_mesh` y `compare::diff` usan `as_chunks` / `as_chunks_mut` (estables desde Rust 1.88, por debajo del mínimo declarado, 1.90).
- `needless_range_loop` (2): una prueba de matrices recorre con iteradores.

**Pendiente de confirmar con la CI:** que Windows (cuyas pruebas nunca se han ejecutado con la suite actual) y wgpu (compilación y comparación de imagen) pasen, y que el estable más reciente quede sin avisos. *(Resultado: véase la 0.4.3.)*

## 0.4.1 — Verificación externa
Primera lectura real de la integración continua de GitHub. Resultado: **corría, y estaba en rojo desde la 0.3.0** (la 0.1.0 pasó en verde). En la 0.4.0 los tres sistemas fallaban en Clippy antes de llegar a las pruebas, y el trabajo de wgpu fallaba al compilar. Esta versión no añade funciones: arregla lo que se pudo arreglar sin ver los registros, y cambia la CI para que los registros no hagan falta.

**CI (`ci/ci.yml`, se copia a `.github/workflows/ci.yml`)**
- Reescrita. El compilador de los trabajos que bloquean está **fijado a 1.97.0** (el mismo con el que se desarrolla); un trabajo aparte prueba el **compilador mínimo declarado** (1.90) y otro, **informativo y sin bloquear**, el **estable más reciente** para enterarse de avisos nuevos de Clippy antes de subir la versión fijada. Solo se usan acciones de `actions/*` y `rustup`: ninguna acción de terceros.
- Cada paso corre aunque el anterior falle (antes, un Clippy en rojo ocultaba las pruebas de Windows y macOS), con plazos para que nada se cuelgue horas.
- **`ci/annotate.py`**: ejecuta cada paso y, si falla, publica el error como **anotación** (con archivo y línea para Clippy y el compilador, y un resumen con todos los diagnósticos, las líneas de pánico y el final del registro). GitHub solo enseña el texto de los registros a quien ha iniciado sesión; las anotaciones se leen sin credenciales por la API. Los pasos de humo y de pruebas publican además un resumen de lo que midieron.
- El trabajo de wgpu ahora también ejecuta `gpu_render` y publica la diferencia entre la imagen de la GPU y la del rasterizador.

**wgpu (`gpu/tsofgw-wgpu`): dos errores de compilación seguros**, encontrados leyendo la documentación de wgpu 30.0.2:
- `PollType::wait()` ya no existe; se usa `wait_indefinitely()`.
- `BufferSlice::get_mapped_range()` devuelve `Result`; se maneja y el búfer se desmapea pase lo que pase.
- Además: la espera de la lectura de píxeles tiene un plazo (30 s) en vez de poder colgarse; las mallas sin índices no se dibujan (wgpu 30 no admite búferes vacíos en `set_*_buffer`).
- La maqueta con la que se comprobaba este crate asumía las firmas viejas, por eso no los detectó: ahora refleja las firmas verificadas en docs.rs.

**Seguridad visible**
- README: aviso al principio, en español e inglés, de que **no es apto para exponerse a internet** (sin cifrado en los dos canales, la clave de clúster permite suplantar a cualquier shard, sin persistencia, sin directorio de sesiones).
- `SECURITY.md` nuevo: qué protege y qué no cada canal, uso seguro mientras tanto, plan y cómo avisar de una vulnerabilidad.
- `tsofgw serve` avisa por la salida de error cuando algún canal (jugadores o bus) escucha fuera del bucle local, que incluye la dirección por defecto `0.0.0.0`.
- `.gitignore` excluye `*.key`, `*.pem` y `*.secret` (un `git add -A` subía la clave que crea `tsofgw keygen -o master.key` en la carpeta del repositorio), `target/` en cualquier carpeta (el de `gpu/` no estaba cubierto) y los `desktop.ini`/`Thumbs.db`/`.DS_Store` que crean los sistemas operativos (había un `desktop.ini` subido por error).

**Sin cambio de comportamiento:** `varint_len` se reescribió con la misma cuenta por otra fórmula (`(70 - ceros) / 7`) y una aserción de `partition.rs` usa `is_empty()`; ambas por prudencia ante lints nuevos, cubiertas por las pruebas de tamaño exacto que ya existían.

**Pendiente de confirmar con la CI:** que Clippy quede en verde, que las pruebas pasen en Windows y macOS (nunca se ejecutaron allí), que wgpu compile y que su imagen coincida con la del software. *(Resultado: véase la 0.4.2.)*

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
