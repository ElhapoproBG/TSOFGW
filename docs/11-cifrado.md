# Cifrado de la red (versión 0.6.0)

Hasta la 0.5 los paquetes de los jugadores y los del bus entre shards iban **autenticados pero en claro**: nadie
podía alterarlos sin que se notara, pero cualquiera que mirase la red los leía. Desde la 0.6.0 los dos canales
van **cifrados y autenticados** con ChaCha20-Poly1305 (RFC 8439).

> **Esto no hace a TSOFGW apto para exponerse a internet.** Cierra un agujero (la lectura pasiva del tráfico) y deja
> abiertos otros, listados en la sección 6. El cifrado es **código propio, sin auditar**: léase la sección 2.

## 1. Qué garantiza y qué no

| | Con la 0.6.0 |
|---|---|
| Leer el tráfico de los jugadores o del bus | **No se puede** sin la clave de la cuenta o la de clúster: la carga va cifrada |
| Alterar, repetir, reflejar o redirigir un paquete | Se detecta (la cabecera entera va autenticada) |
| Saber que dos máquinas hablan, cuántos paquetes, de qué tamaño y a qué ritmo | **Sí se puede**: el tamaño y el ritmo no se ocultan |
| Saber qué cuenta se conecta | **Sí se puede**: el número de cuenta viaja en claro en el `AUTH` del handshake (autenticado, no cifrado) |
| Descifrar capturas antiguas si más tarde se filtra una clave | **Sí se puede**: no hay secreto hacia adelante (las claves de sesión salen de una clave compartida y de dos nonces) |
| Que la clave de clúster siga siendo confianza total | **Sigue siéndolo**: quien la tenga descifra todo el bus y habla como cualquier shard o como el directorio |
| Cifrado en reposo (disco) | **No**: lo guardado sigue autenticado, no cifrado |

## 2. La primitiva: `tsofgw-aead`

Un crate nuevo, sin dependencias, con `#![forbid(unsafe_code)]`: ChaCha20 (bloque y flujo), Poly1305 (limbs de
26 bits) y la construcción AEAD del RFC 8439 §2.8 (`seal_in_place`, `open_in_place`, `seal`, `open`). `open_in_place`
**verifica la etiqueta antes de descifrar** y, si falla, deja el búfer intacto.

**Por qué es código propio.** El proyecto no tiene dependencias externas y en esta fase no hay forma de añadir una
biblioteca de criptografía. En las notas de la 0.5 se prometió «una biblioteca auditada»; no se ha podido cumplir y
lo digo aquí sin adornos: **ChaCha20-Poly1305 de este crate lo escribí yo y nadie lo ha auditado.**

**Qué lo respalda** (no es lo mismo que una auditoría):
- Los vectores del propio RFC 8439 (§2.3.2, §2.4.2, §2.5.2, §2.6.2 y §2.8.2), comprobados además contra OpenSSL.
- **Vectores de una implementación independiente** (OpenSSL 3, a través de la biblioteca `cryptography` de Python):
  23 de AEAD con longitudes en los bordes de bloque (0, 1, 15, 16, 17, 63, 64, 65…) y datos asociados de distintos
  tamaños, **458 de Poly1305** (claves y mensajes extremos, y reducciones finales forzadas a propósito) y 6 bloques de
  ChaCha20 con contadores extremos. Se generan con `ci/generar_vectores_aead.py` (semilla fija, reproducible), y **la CI
  los regenera con OpenSSL en cada ejecución y exige que sean idénticos a los guardados**.
- Una prueba de **30 000 casos aleatorios** contra OpenSSL, ejecutada una vez en modo de depuración (con comprobación de
  desbordamientos aritméticos) y en modo optimizado, con los 30 000 iguales. No se conserva en el repositorio.
- Propiedades: ida y vuelta para todas las longitudes de 0 a 140; **cada bit** del texto cifrado, de la etiqueta, de los
  datos asociados, del nonce y de la clave que se altere hace fallar la apertura; truncar o alargar falla; la etiqueta
  que falla no toca el búfer; el contador de bloque de ChaCha20 no da la vuelta (entra en pánico en vez de repetir flujo).
- **La prueba de las pruebas** (`ci/mutaciones.py`, mutantes C1 a C12): 12 defectos inventados que un programador
  podría cometer en el cifrado o en su uso. **Los 12 se detectan** (ver sección 8 sobre el primero).

**Lo que no se ha comprobado:**
- **Tiempo constante.** ChaCha20 y la aritmética de Poly1305 no tienen ramas ni accesos a memoria que dependan de los
  datos secretos, y la comparación de la etiqueta no sale antes de tiempo. Pero **no lo he medido** en ninguna CPU ni con
  una herramienta de análisis; el compilador podría, en teoría, introducir una rama.
- **Borrado de claves.** No se ponen a cero al soltarlas (haría falta `unsafe` o una dependencia): quedan en memoria
  hasta que se reutiliza.
- **Revisión por un criptógrafo.** Ninguna.

El crate está aislado a propósito: su interfaz es la de cualquier AEAD (`seal_in_place` / `open_in_place` con clave de 32
bytes, nonce de 12 y etiqueta de 16), así que **cambiarlo por una biblioteca auditada** (`chacha20poly1305` de RustCrypto,
por ejemplo) es sustituir un crate sin tocar el transporte ni el bus. Es lo que recomiendo hacer en cuanto haya red de
dependencias, antes de exponer nada.

**Coste medido** (sandbox de 2 núcleos, ruidoso): 322 MB/s, unos 3,7 µs por paquete de 1180 bytes y 0,4 µs por uno de
40. En una prueba de carga con 60 bots y 2 000 NPC, cinco pasadas alternas de la 0.5.1 y de la 0.6.0: tick medio de
388–604 µs sin cifrado y 358–419 µs con él. **Dentro del ruido: no se aprecia coste.**

## 3. Transporte de jugadores

```text
DATA       0x10 | sesión(4) | contador(8) | msg_id(2) idx(1) total(1) | carga CIFRADA | etiqueta(16)
DISCONNECT 0x11 | sesión(4) | contador(8) | etiqueta(16)
```
- **Claves.** En el handshake, las dos puntas derivan de la clave de la cuenta, los dos nonces y el identificador de
  sesión una clave maestra de sesión, y de ella **una clave AEAD por sentido** (`aead-c2s`, `aead-s2c`). Cada sesión
  estrena claves.
- **Nonce.** 4 bytes a cero y el contador de 64 bits del paquete. Como cada sentido de cada sesión tiene su propia clave
  y el contador solo crece (empieza en 0 y no se reutiliza ni en los DISCONNECT), **no se repite nunca una pareja
  (clave, nonce)**. Agotar 2^64 paquetes es imposible a 1200 bytes por datagrama.
- **Datos asociados.** La cabecera completa (tipo, sesión, contador, `msg_id`, `idx`, `total`): visible, pero autenticada.
  Mover un paquete a otra sesión o cambiarle el contador para esquivar el anti-replay lo invalida.
- **El tamaño no crece.** La etiqueta de 16 bytes ocupa el mismo sitio que la del HMAC anterior.
- **Sin vuelta atrás.** El protocolo pasa a la **versión 2** y no hay modo sin cifrar. Un cliente de la 0.5 no se
  entiende con un servidor de la 0.6 (el servidor ignora su `HELLO`, que lleva la versión 1): no puede forzarse un
  paquete en claro. Hay que actualizar clientes y servidores juntos.
- **El handshake no se cifra.** `HELLO`, `CHALLENGE`, `AUTH` y `ACCEPT` siguen autenticados con HMAC-SHA256 y en claro:
  el número de cuenta es visible.

## 4. Bus entre shards

```text
BUS 0x20 | versión(1)=2 | desde(2) | hacia(2) | arranque(4) | contador(8) | reloj_ms(8) | config(8) | carga CIFRADA | etiqueta(16)
```
- **Una clave por emisor**: `HMAC(clave del bus, "tsofgw/bus-aead/v2" ‖ desde)`. Dos shards nunca cifran con la misma
  clave, y un datagrama del shard 0 no puede hacerse pasar por uno del 1 (cambiar `desde` cambia la clave y el dato
  asociado).
- **Nonce** = `arranque ‖ contador`. El contador de cada emisor arranca en el reloj de pared (µs) y crece en cada envío;
  `arranque` es una **sal aleatoria de 32 bits** que cada proceso elige al arrancar, de `/dev/urandom` o `BCryptGenRandom`.
  Sin la sal, un shard que se reiniciase con el reloj retrasado (NTP, una máquina virtual restaurada) podría repetir un
  nonce con la misma clave, y repetirlo revela el XOR de dos mensajes y deja falsificar. Con la sal, además del reloj
  retrasado haría falta la **misma** sal (probabilidad 2^-32 por reinicio). **Es un riesgo residual, no cero.**
- La cabecera crece de 30 a 34 bytes y `MAX_PEER_MSG` baja de 1154 a 1150 (el datagrama sigue siendo de 1200). Los
  fantasmas se trocean por tamaño, así que se adaptan solos.
- `BUS_VERSION` pasa a 2: un shard de la 0.5 cuenta como «formato imposible» los datagramas de la 0.6 y al revés. Hay que
  actualizar **todo el clúster a la vez**. La huella de configuración (`tsofgw/cluster-config/v2`) no cambia.
- Sigue valiendo lo de la 0.5: anti-replay por emisor, frescura del reloj (±30 s), el bus nunca responde.

## 5. Cómo se probó

- **Cifrado** (`tsofgw-aead`): 10 pruebas unitarias y 3 de integración con los vectores de OpenSSL (sección 2).
- **Transporte**: 5 pruebas del formato (ida y vuelta, **cada bit** de DATA y de DISCONNECT, un DISCONNECT que no se hace pasar
  por DATA, nonces distintos para 2 000 contadores, sesión y contador autenticados) y 6 de extremo a extremo: en una red
  simulada, **ningún trozo de 8 bytes del contenido aparece en ningún datagrama**, los tamaños no crecen, el mismo mensaje
  enviado dos veces no se parece, un paquete de un sentido no vale en el otro, un atacante que conoce la cuenta pero no la
  clave no forja DATA ni DISCONNECT, y un `HELLO` de la versión anterior se ignora; y sobre **UDP real con un espía
  interpuesto** que reenvía todo y lo guarda, el juego funciona (mensajes de 5 000 y 3 000 bytes, fragmentados) y el espía
  no lee nada.
- **Bus**: ida y vuelta, cada bit de un datagrama, clave por emisor, la sal de arranque cambia el cifrado, un datagrama de la
  versión 1 se descarta como formato, y un **espía entre dos shards reales** (UDP en el bucle local) no lee nada y lo
  reenviado le llega entero al shard de verdad. Las pruebas del clúster (tormenta de caídas, procesos a los que se hace
  `kill -9`, intruso con otra clave) pasan sin cambios de fondo: cifrado y persistencia conviven.
- **Defectos inventados** (`ci/mutaciones.py`): C1–C12 (poner mal la reducción final de Poly1305, no rellenar los datos
  asociados, descifrar aunque falle la etiqueta, dejar dar la vuelta al contador de ChaCha20, aceptar DATA o DISCONNECT sin
  comprobar la etiqueta, ignorar el contador en el nonce, usar la misma clave en los dos sentidos, aceptar un `HELLO` de
  cualquier versión, usar la misma clave de bus para todos los shards, ignorar la sal de arranque, aceptar datagramas del
  bus sin comprobar la etiqueta): **12 de 12 detectados**.

## 6. Qué sigue abierto

- **Sin secreto hacia adelante**: haría falta un intercambio de claves efímero (X25519). No lo he escrito: es criptografía
  asimétrica y mi regla desde el principio es no implementarla a mano sin una biblioteca auditada.
- **Credenciales por clave compartida**: la clave maestra suplanta a cualquier cuenta; la clave de clúster, a cualquier shard
  y al directorio, y **descifra todo el bus**.
- **Metadatos visibles**: la cuenta que se conecta, el tamaño y el ritmo de los paquetes, qué shards hablan.
- **El disco sigue sin cifrar** y no se detecta el retroceso de una copia vieja.
- **Ataques volumétricos** (inundar de paquetes): no hay defensa más allá de los límites por sesión.
- **Criptografía propia sin auditar** (sección 2) y **sin pruebas de tiempo constante**.
- Todo medido en bucle local y en ejecutores compartidos.

## 7. Dónde cambia el código

`crates/tsofgw-aead` (nuevo), `tsofgw-transport` (`crypto.rs`: claves de sesión AEAD; `wire.rs`: `build_data`, `open_data`,
`build_disconnect`, `verify_disconnect`; protocolo versión 2), `tsofgw-online/src/udp_bus.rs` (`BusKey`, `Header`,
`seal`/`open`, sal de arranque; `BUS_VERSION` 2), `ci/generar_vectores_aead.py`, y 12 mutantes nuevos en `ci/mutaciones.py`.

## 8. Un hallazgo de la prueba de las pruebas

La primera versión del catálogo dejó **sobrevivir** al mutante C1 («Poly1305 no hace la reducción final módulo 2^130-5»):
ninguna de mis pruebas lo notaba. Es un hueco real en la suite y de los que importan: esa reducción solo cambia el resultado
cuando el acumulador acaba entre 2^130-5 y 2^130-1, algo que un mensaje aleatorio casi nunca provoca (probabilidad del orden de 2^-127).
Los vectores aleatorios de OpenSSL y los 30 000 casos aleatorios pasaban igual con el defecto. Se cerró construyendo a propósito
claves con `r` pequeño y mensajes de bloques `0xFF` (como los vectores A.3 del RFC 8439): 378 vectores más, y con ellos el
mutante se detecta. **El código no tenía ese defecto**; lo que faltaba era una prueba que lo demostrase.
