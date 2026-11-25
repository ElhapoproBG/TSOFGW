# Seguridad

> ## ⚠️ TSOFGW 0.5 no es apto para exponerse a internet
> *Not suitable for exposure to the internet.* Es un prototipo para desarrollo, pruebas y redes de confianza.

Este documento dice, sin adornos, qué protege hoy el motor, qué no, cómo usarlo con seguridad mientras tanto y
qué falta. Nada de lo que describe ha sido auditado por terceros.

## En una frase

Úsalo en bucle local o en una red privada de confianza. Si el tráfico tiene que cruzar una red que no lo es,
métalo en una VPN (WireGuard, IPsec). **No abras sus puertos a internet.**

## Qué protege hoy y qué no

| Canal o pieza | Protege | **No** protege |
|---|---|---|
| **Jugadores ↔ servidor** (UDP) | Autenticación mutua con una clave por cuenta; cookie atada a la dirección (sin suplantación de origen ni amplificación); HMAC-SHA256/128 por paquete con clave distinta por sentido y sesión; ventana anti-replay; límites de paquetes, sesiones y entradas por tick. | **Confidencialidad**: los paquetes viajan en claro, solo autenticados. Quien observe la red lee posiciones, entradas y estado del mundo. Sin secreto hacia adelante. Credenciales por clave compartida (no asimétricas). Ataques volumétricos. |
| **Bus entre shards** (UDP) | HMAC-SHA256/128 con una clave derivada de la clave de clúster; anti-replay por origen; reloj con tolerancia de ±30 s; huella de configuración; el bus nunca responde (no amplifica). | **Confidencialidad**, igual que arriba. **La clave de clúster es un único secreto compartido por todos los shards y el directorio: quien la tenga puede hacerse pasar por cualquier shard o por el directorio** (inyectar NPC y jugadores, redirigir clientes, reasignar cuentas). No hay identidad por shard ni revocación individual. |
| **Cuentas** | El servidor es autoritativo: el cliente solo envía intenciones. Con el directorio de sesiones, una cuenta vive en un solo shard: el cambio de dueño es una operación atómica sobre una época, anotada en disco antes de responder. | **Quien tenga la clave maestra puede suplantar a cualquier cuenta.** El directorio es **un solo proceso**: sin alta disponibilidad, sin arrendamientos y sin sustitución automática. Sin directorio, la unicidad de una cuenta es de mejor esfuerzo. |
| **Estado en disco** | Diario y checkpoints con HMAC-SHA256 (clave derivada de la clave de clúster o de la maestra, distinta por carpeta): un daño del disco o una edición sin la clave se detecta; un shard no arranca con la carpeta de otro mundo, de otro shard o con otra clave, y no borra nada al negarse. | **No está cifrado**: quien lea la carpeta ve cuentas y posiciones. **No se detecta el retroceso**: restaurar una copia vieja de la carpeta no se nota (la clave autentica el contenido, no la frescura). Un daño en los últimos 8 MiB del diario se toma por una escritura a medias y se descarta sin error. Quien tenga la clave puede fabricar un almacén válido. |
| **Módulos** | Manifiesto con SHA-256, firma, niveles de confianza, permisos declarados y verificados al cargar. | Las firmas son HMAC con clave compartida (sirven para pruebas y redes privadas, **no** para distribución pública). No hay sandbox en tiempo de ejecución: los permisos se declaran y se auditan, pero no se aíslan. |
| **Cliente** | Se trata como no confiable. | Sin atestación del cliente; trampas que no alteran el protocolo (ayudas de puntería) quedan fuera. |

Lo que tampoco ha pasado:
- **Auditoría externa o revisión criptográfica.** SHA-256 y HMAC son implementación propia del proyecto,
  comprobada con los vectores oficiales (NIST y RFC 4231), pero sin auditar. No se implementó criptografía
  asimétrica propia a propósito: llegará con una biblioteca auditada.
- **Fuzzing continuo.** Hay pruebas con mensajes hostiles, que no es lo mismo.
- **Pruebas fuera de una máquina.** El clúster solo se probó en bucle local.

## Uso seguro mientras tanto

- Escucha en `127.0.0.1` o en una red privada. `tsofgw serve` avisa por la salida de error cuando algún canal
  escucha fuera del bucle local (la dirección por defecto, `0.0.0.0`, cuenta como fuera).
- Genera las claves con `tsofgw keygen -o ARCHIVO` (aleatorias; permisos `0600` en Unix) y pásalas **solo por
  archivo** (`--master-file`, `--cluster-key-file`). La herramienta no acepta claves en la línea de comandos a
  propósito: se verían en la lista de procesos y en el historial del intérprete.
- No guardes las claves en el repositorio. El `.gitignore` del proyecto excluye `*.key`, `*.pem` y `*.secret`,
  pero no sustituye a la prudencia.
- La carpeta de datos (`--data-dir`) contiene las cuentas y sus posiciones, sin cifrar: dale permisos solo para el
  usuario del servidor y no la pongas en un disco compartido. Haz copias **con el proceso parado** y restaura **todo el
  cluster a la vez** (una carpeta restaurada sola deja sin estado a las cuentas que el directorio sitúa en ese shard
  desde la copia —no podrán entrar— y conserva copias viejas de las que ya se fueron; el motor no lo detecta). El directorio también es crítico: protege su carpeta como las de los shards.
- No reutilices en un despliegue real las claves que usaste para probar, y no compartas la clave de clúster
  con quien no deba poder hacerse pasar por todos los shards.
- Trata los módulos sin firma como código no confiable: cargan solo con una bandera explícita y nunca reciben
  `accounts`, `system` ni `disk_write`.

## Plan

En orden de prioridad (el detalle técnico está en los documentos 03, 08, 09 y 10; la persistencia y el directorio de
sesiones, que eran el primer punto del plan, llegaron en la 0.5.0):

1. **Cifrado autenticado (AEAD) de los dos canales** con una biblioteca auditada, con claves de sesión por
   intercambio de claves y secreto hacia adelante. Es lo que hoy impide exponerlo a internet.
2. **Directorio de sesiones replicado** (varios procesos con elección de líder y arrendamientos) para que deje de ser
   un punto único de fallo, **detección de retroceso** del almacén (un ancla fuera de la máquina) y **cifrado en reposo**
   de lo que se guarda en disco.
3. **Identidad por shard** (una clave por shard, con firmas asimétricas y revocación individual) para que la
   clave de clúster deje de ser un punto único de confianza, y **credenciales de cuenta asimétricas** en lugar de
   derivarlas de una clave maestra.
4. **Firmas Ed25519 de módulos** y **sandbox WebAssembly**.
5. **Fuzzing continuo** de los analizadores de paquetes y del bus, y una **auditoría externa** antes de
   recomendar su uso público.

## Cómo avisar de una vulnerabilidad

Usa «Report a vulnerability» en la pestaña *Security* del repositorio (aviso privado) y no publiques detalles en
un *issue* abierto. Es un prototipo mantenido sin plazos garantizados, pero cada aviso se atiende y se acredita.

## Versiones con soporte

Solo la última.
