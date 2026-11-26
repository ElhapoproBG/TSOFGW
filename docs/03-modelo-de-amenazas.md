# Modelo de amenazas y confianza

## Amenazas
- **A1. Fork malicioso que se hace pasar por oficial** (patrón visto en npm/PyPI).
- **A2. Código malicioso dentro del proyecto oficial** (ingeniería social a mantenedores, estilo xz Utils).
- **A3. Dependencia comprometida** (cadena de suministro).
- **A4. Módulo de terceros que abusa de permisos.**
- **A5. Cliente alterado que engaña a la red de cuentas compartidas.**

## Mitigaciones
- A1: marca registrada, canal único de distribución, hashes y firmas publicados, builds reproducibles. Módulos sin firma cargan solo con confirmación explícita y quedan marcados en cliente y servidor.
- A2: revisión por dos personas obligatoria (más estricta en red, cuentas y carga de módulos); firmas de release con claves en hardware y varios firmantes; revocación de versiones.
- A3: dependencias mínimas fijadas por hash, SBOM, análisis automático en CI.
- A4: capacidades declaradas por nodo + sandbox (WASM). Pedir un permiso nuevo en una actualización exige revisión.
- A5: la autoridad real vive en el servidor; la atestación del cliente es una señal, nunca la única defensa.

## Niveles de confianza de módulos
Oficial · Certificado por terceros (auditado) · Sin firma (explícito y visible).

## Red (0.3.0)
Cubre A5 en el lado del servidor y añade amenazas de red; detalle en el documento 08.
- **Suplantación de origen y amplificación:** cookie atada a la dirección y respuestas menores que la petición; sin estado reservado antes de AUTH.
- **Paquetes alterados o repetidos:** etiqueta por paquete (desde la 0.6, la de ChaCha20-Poly1305, con la cabecera autenticada) y ventana anti-replay.
- **Escucha pasiva (0.6.0):** la carga de cada paquete va cifrada con una clave distinta por sentido y sesión; un espía interpuesto sobre UDP real no lee nada. Ve el número de cuenta del handshake, el tamaño y el ritmo. Detalle y límites en el documento 11.
- **Cliente tramposo:** el servidor es autoritativo, el cliente solo envía intenciones, la velocidad la impone la simulación y un sistema de créditos limita los inputs por tick; los mensajes malformados llevan a expulsión.
- **Inundación:** límite de paquetes por segundo y por sesión, de sesiones y de sesiones pendientes.
- **Captura hoy, clave filtrada mañana (0.7.0):** el canal de los jugadores tiene secreto hacia adelante (X25519 efímero en el handshake, claves de sesión que dependen del secreto de Diffie-Hellman).
- **No cubierto aún:** que el cifrado y X25519 son propios y sin auditar, y el intruso activo que ya tenga la clave de la cuenta, credenciales asimétricas, ataques volumétricos de red (eso se mitiga fuera del motor), trampas que no alteran el protocolo (por ejemplo, ayudas de puntería) y atestación del cliente.

## Clúster de shards (0.4.0)
Añade un segundo canal de red, el **bus entre shards**, y con él nuevas amenazas; detalle en el documento 09.
- **Intruso en el bus** (forja, altera o repite datagramas): todos van cifrados y autenticados (desde la 0.6, ChaCha20-Poly1305 con una clave por shard emisor derivada de la clave de clúster; hasta la 0.5, HMAC-SHA256/128 en claro), con ventana anti-replay por origen y una marca de reloj con tolerancia de ±30 s; un intruso sin la clave no consigue nada y queda registrado (probado sobre sockets reales). El bus nunca responde a lo que recibe y solo envía a las direcciones configuradas, así que no sirve para amplificar ni reflejar.
- **Shard mal configurado o con otro mundo:** cada datagrama lleva una huella de la configuración que debe coincidir (rejilla, tick, radios, NPC, semilla); si no coincide se rechaza y se cuenta aparte.
- **Cliente que intenta aprovechar un traspaso:** el estado del jugador lo envía el shard de origen, nunca el cliente; el destino solo adopta un `HANDOVER` del mismo shard que lo preparó, para esa cuenta, y después de que el cliente se autenticara con su propia clave. La autoridad cambia en un único mensaje.
- **Reinicio de un shard con capturas antiguas:** el contador del bus parte del reloj, así que sigue creciendo tras un reinicio y las capturas previas no se aceptan.
- **Escucha pasiva del bus (0.6.0):** cifrado, con un espía interpuesto entre dos shards reales que no lee nada. **Sigue sin secreto hacia adelante** (claves estáticas).
- **No cubierto aún:** **la clave de clúster es un punto único de confianza** (quien la tenga descifra el bus y puede suplantar a cualquier shard y, desde la 0.5.0, al directorio de sesiones). El directorio de sesiones y la persistencia que faltaban aquí llegaron en la 0.5.0 (sección siguiente).

## Persistencia y directorio de sesiones (0.5.0)
Añade datos en disco y un nuevo proceso con autoridad sobre las cuentas; detalle en el documento 10.
- **Una cuenta en dos shards, o en ninguno:** el cambio de dueño es una operación atómica del directorio sobre una época (*compare-and-swap*), anotada en disco antes de responderse; el origen no envía el estado hasta tenerla anotada y el destino no acusa hasta haberlo escrito. Un `HANDOVER` repetido, tardío o con una época que el destino ya conoce no resucita ni hace retroceder a nadie. Se vigila durante toda la marcha de las pruebas, con caídas en cada punto del traspaso.
- **Carpeta de datos alterada por alguien sin la clave:** cada registro y cada checkpoint llevan un HMAC-SHA256 (la secuencia y la generación entran en la etiqueta); un daño o una edición se detecta, y la carpeta de otro mundo, de otro shard o con otra clave se rechaza sin tocarla.
- **Tampoco se detecta lo que parece una escritura a medias:** un cambio en los **últimos 8 MiB del diario** es indistinguible de una caída y se descarta sin error (la señal es `torn_bytes` mayor que cero sin que hubiera caída).
- **Copia vieja devuelta a su sitio (retroceso):** **no se detecta**. La clave autentica el contenido, no la frescura: quien pueda reemplazar la carpeta de un shard puede devolver el estado de ayer. Hace falta un ancla fuera de esa máquina (pendiente).
- **Confidencialidad en reposo:** **no hay cifrado**; la carpeta contiene cuentas y posiciones en claro, autenticadas.
- **Directorio comprometido o suplantado:** quien tenga la clave de clúster puede enviar `DirRoute` y `DirResult` falsos (reasignar cuentas, hacer que un shard crea que una cuenta es suya). Es la misma confianza total de siempre, con más alcance.
- **Directorio caído:** nadie puede entrar ni cruzar bordes con una cuenta mientras no vuelva (lo que ya está jugando sigue jugando y no se pierde a nadie). Por omisión es un solo proceso; con réplicas (0.8.0, documento 12) la caída de una minoría no lo detiene, la de la mayoría sí, igual que antes, y sin pérdida ni duplicados.
- **Disco que falla:** el shard (o el directorio) deja de prometer lo que no está en disco y hay que reiniciarlo; nunca miente sobre lo escrito.

## Estado de implementación del sistema de módulos (0.1.0)
Implementado en `tsofgw-pack` y probado:
- **Manifiesto de módulo** con nombre, versión, permisos y SHA-256 del payload; formato estricto (claves desconocidas, duplicadas o faltantes se rechazan).
- **Niveles de confianza** (oficial, certificado, sin firma) y política de permisos por nivel; los módulos sin firma solo cargan con una bandera explícita y nunca reciben `accounts`, `system` ni `disk_write`.
- **Verificación** de hash, firma y revocación. Un firmante desconocido o una firma inválida se rechazan: no se degrada en silencio a "sin firma".
- **Coherencia de permisos:** un nodo no puede usar permisos que su manifiesto no declaró.
- **Herramienta** `tsofgw pack` y `tsofgw verify`; las claves se leen de archivo, nunca de la línea de comandos.

Pendiente:
- **Firmas asimétricas (Ed25519).** Hoy solo hay HMAC con clave compartida, adecuado para pruebas y redes privadas pero no para distribución pública. Se añadirá con una dependencia auditada.
- **Sandbox en tiempo de ejecución (WebAssembly)** que aplique los permisos declarados.
- **Builds reproducibles, SBOM y firmas de release con varios firmantes.**
