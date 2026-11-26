# Directorio de sesiones replicado con Raft (versión 0.8.0)

Hasta la 0.7 el directorio de sesiones —el que decide en qué shard vive cada cuenta— era **un solo proceso**: si
caía, nadie podía entrar ni cruzar bordes con una cuenta hasta que volviera (lo que ya estaba jugando seguía
jugando). La 0.8.0 lo convierte, **si se quiere**, en un grupo de 3, 5 o 7 réplicas puestas de acuerdo con
**Raft**: la caída de una minoría (1 de 3, 2 de 5, 3 de 7) no detiene a nadie más que un instante.

Con una sola réplica todo funciona exactamente como en la 0.5–0.7 (mismo código, mismo formato de disco).

## 1. Qué problema resuelve y cuál no

| | Sin réplicas (0.5–0.7) | Con réplicas (0.8.0) |
|---|---|---|
| Cae el proceso del directorio | Nadie entra ni cruza bordes hasta que vuelva | Con mayoría viva: un corte mientras se elige otro líder (plazo de elección de 300–600 ms más unas rondas de red; así configurado, no medido por separado) |
| Cae una minoría de réplicas | — | Ni se nota |
| Caen la mayoría | — | Se detienen los traspasos y las entradas (como antes); nada se pierde ni duplica |
| Se pierde un disco | Hay que reconstruir desde los shards | Esa réplica se rellena sola desde las otras |
| Se pierden todos los discos | Reconstruir desde los shards | Igual (`store rebuild-directory` / `queue_import`) |

**Lo que NO resuelve:** la clave de clúster sigue siendo un único secreto compartido (quien la tenga puede
hacerse pasar por una réplica); las réplicas no están en máquinas distintas a menos que tú las pongas ahí; el
bus entre réplicas no tiene secreto hacia adelante (como el resto del bus, documento 11); el disco sigue sin
cifrar; no se detecta que una réplica vuelva con una copia **vieja** de su carpeta (ver §7).

## 2. Diseño

**Estado.** El mismo de siempre: `cuenta → (shard, época)`. Las operaciones son las de siempre
(`DirLogin`, `DirTransfer`), pero ahora cada petición de un shard se convierte en un **mandato** de 13 o 25
bytes que el líder añade a su diario. Cuando una mayoría lo ha guardado en disco, **todas las réplicas lo aplican
en el mismo orden**. El resultado de un mandato depende solo del estado y del mandato (por eso es el mismo en
todas), y el líder —solo él, y solo de lo que él propuso en su liderazgo actual— contesta al shard.

**Raft** (`crates/tsofgw-online/src/raft.rs`, sin entrada/salida: no abre archivos, sockets ni relojes):

- elección con **pre-votación** (una réplica aislada no sube el término de las demás al volver);
- **protección de líder** (se ignoran votaciones mientras se oye a un líder vivo) y **comprobación de quórum**
  (un líder que deja de ver a la mayoría abdica);
- replicación por lotes; una entrada solo se confirma contando réplicas si es **del término actual** (§5.4.2
  del artículo: evita el error de la «figura 8»);
- **compactación** del diario y **envío de instantáneas** por trozos a la réplica que se quedó más atrás de lo
  compactado;
- **primero al disco, después al mundo**: todo lo que Raft pide guardar (voto, término, entradas, truncados)
  se sincroniza *antes* de enviar ningún mensaje ni contestar a ningún shard. Una réplica nunca confirma una
  entrada ni concede un voto que luego pueda olvidar.

**En el almacén de cada réplica** (el mismo almacén autenticado de siempre): `m/repl` (cuántas réplicas y cuál es:
una carpeta no se reutiliza con otra identidad), `r/meta` (término y voto), `r/base` (hasta dónde se compactó),
`r/applied` (hasta dónde se aplicó, escrito en el **mismo lote** que los cambios de cuentas que provoca),
`r/e/<índice>` (entradas) y `a/<cuenta>` (el estado). Una carpeta de un directorio sin réplicas no se abre como
réplica ni al revés.

**Los shards.** Los identificadores del bus son `0..N` para los shards y `N..N+R` para las réplicas. Cada shard
tiene una *pista* de quién lidera: manda ahí sus peticiones; una réplica que no lidera contesta
`DirRedirect{líder}` (o calla si no lo sabe todavía) y, si varias peticiones seguidas no reciben respuesta, el
shard prueba con la siguiente réplica. Los reintentos ya eran seguros desde la 0.5 (las operaciones son
idempotentes y por época), y el líder además no vuelve a proponer lo que ya tiene en vuelo.

## 3. Cómo se usa

```bash
# Tres réplicas del directorio, cada una con su carpeta y su dirección de bus (las mismas tres en todas partes):
R=127.0.0.1:7100,127.0.0.1:7101,127.0.0.1:7102
tsofgw directory --grid 2x1 --peers $P --cluster-key-file c.key --replicas $R --replica 0 --data-dir dir0
tsofgw directory --grid 2x1 --peers $P --cluster-key-file c.key --replicas $R --replica 1 --data-dir dir1
tsofgw directory --grid 2x1 --peers $P --cluster-key-file c.key --replicas $R --replica 2 --data-dir dir2

# Los shards reciben la lista entera en lugar de una sola dirección:
tsofgw serve ... --shard 0 --grid 2x1 --peers $P --data-dir datos0 --directory $R

# En simulación, con tormenta de caídas (también caen réplicas, a veces la líder y, con 5, dos a la vez):
tsofgw loadtest --shards 2x2 --clients 60 --seconds 30 --persist --directory --dir-replicas 3 --chaos 8
```

El grupo debe arrancarse con las **mismas** réplicas, en el mismo orden, en todos los shards y réplicas; la
huella de configuración del bus incluye el número de réplicas, así que quien venga con otro número se descarta.
Cada réplica imprime cada 5 s su papel (`LÍDER`/`seguidora`), término, hasta dónde aplicó y cuántas peticiones
propuso, redirigió o no pudo atender.

## 4. Cómo se probó

- **Simulación de Raft sola** (`tests/raft.rs`): réplicas con «discos» que se pierden a medias, red con
  pérdidas, duplicados, reordenación y particiones, caídas en pleno guardado; se comprueba que hay como mucho un
  líder por término, que lo confirmado nunca cambia ni se contradice entre réplicas y que, con calma, todas
  convergen. Con 1, 3 y 5 réplicas; instantáneas con la réplica muy atrasada; mensajes mal formados.
- **El cluster completo** (`tests/dir_raft.rs`), con 3 y 5 réplicas: reparto estable y réplicas idénticas;
  caída del líder (se elige otro, los traspasos siguen, la caída vuelve y se pone al día); **sin mayoría** (no se
  completa ningún traspaso nuevo, nada se rompe, con ella todo sigue); réplica atrasada puesta al día **con
  instantánea**; tormenta de caídas (de shards y de réplicas, con pérdidas del 2 % en red y bus); directorio
  replicado perdido entero y reconstruido desde los shards; carpeta con identidad ajena o del otro modo; mensajes
  hostiles; un traspaso de época vieja o pedido por quien no es el dueño se rechaza.
- **Procesos reales** (`tests/cli.rs`): 3 réplicas, 2 shards y bots por UDP; se hace `kill -9` al **líder** y el
  cluster sigue cruzando bordes con las dos que quedan; la réplica vuelve con su carpeta y el cluster sigue; al
  final las tres carpetas tienen las mismas 10 cuentas y los shards cuadran con ellas.
- **Mutaciones:** 14 mutantes nuevos (R1–R14) sobre el protocolo y el directorio: mayoría mal contada, voto sin
  mirar el término del diario, confirmar entradas de términos viejos, doble voto, voto sin guardar, truncado sin
  guardar, entradas sin comprobar el punto anterior, no descartar entradas en conflicto, **no sincronizar el
  disco**, ignorar la época, reiniciar cuentas existentes, instantánea con épocas inventadas y carpetas con
  identidad equivocada. **A la primera sobrevivieron 5 de 14** (R2, R3, R5, R6 y R9): la simulación aleatoria
  tarda demasiado en dar por azar con secuencias tan concretas (un voto a un candidato con el diario más largo pero
  más viejo; un líder nuevo que cuenta réplicas para confirmar una entrada de un término anterior; un voto que no
  llega a disco antes de la respuesta...). Se cerraron con casos «de libro» movidos a mano contra la máquina de estados
  y con una prueba de caída en seco del directorio; ahora se detectan los 14.

## 5. Un detalle que aprendí a la fuerza

Al escribir la prueba con procesos reales falló la tercera fase: nadie cruzaba el borde. No era el directorio. Los
bots patrulleros caminan +4 s y −4 s según el reloj de **su propio proceso**, así que una fase de 12 s los deja
desplazados y la siguiente arranca *alejándose* del borde. Las fases deben durar un número entero de ciclos de 8 s.
Lo cuento porque un fallo de una prueba no es siempre un fallo del sistema, ni al revés: aquí hubo que mirar los
registros de las tres réplicas y de los dos shards para saberlo.

## 6. Coste

Cada petición de un shard cuesta ahora una ida y vuelta de replicación (el líder guarda, las seguidoras guardan,
el líder confirma y responde): en la simulación (4 shards, 60 clientes, 30 s, bus con 8–12 ms de latencia y sin
pérdidas inducidas), el tiempo de un traspaso de jugador pasó de **59 ms de media (máximo 250 ms) sin réplicas a
114 ms (máximo 400 ms) con 3**; los 107–108 traspasos se completaron en ambos casos, ninguno cancelado. Con disco real el coste dominante será el
`fsync` de cada réplica; **no está medido en un disco de producción**. Todo pasa por el diario, también las
consultas de entrada; una entrada de cuenta ya conocida también ocupa una entrada del diario (se compacta).

## 7. Límites conocidos

- **Pertenencia fija:** 3, 5 o 7 réplicas; no se añaden ni se quitan en caliente.
- **Todo por el diario:** no hay lecturas locales (así no hay lecturas rancias, pero cuesta una ronda).
- **La instantánea se arma entera en memoria** en el líder (18 bytes por cuenta) y se envía por trozos de 990 B;
  para decenas de millones de cuentas habría que partirla en disco.
- **Sin detección de retroceso:** si una réplica vuelve con una copia **vieja** de su carpeta, Raft asume que
  lo que dice haber votado y guardado es cierto. Con una sola réplica en esa situación el grupo sigue siendo
  seguro; con una mayoría de copias viejas, no. (Mismo límite que el almacén desde la 0.5: no hay un ancla fuera
  de la máquina.)
- **Un grupo, un cluster:** la carpeta de una réplica no se usa en otro grupo ni con otro tamaño; si se pierde
  más de la mitad de los discos, se reconstruye desde los shards y las réplicas arrancan **vacías** a la vez.
- **Pruebas formales:** no hay; hay simulación, procesos reales y mutantes. El protocolo está escrito desde el
  artículo de Raft y no ha sido revisado por nadie más.
- **Criptografía:** la del bus es la propia y sin auditar (documento 11); un intruso con la clave de clúster
  puede hacerse pasar por una réplica (y, con ello, por una mayoría de votos si suplanta a bastantes).
- **El directorio sigue siendo punto de confianza:** una réplica comprometida con la clave de clúster sigue
  pudiendo proponer mandatos válidos de cualquier shard.
