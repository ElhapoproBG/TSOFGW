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
- **Paquetes alterados o repetidos:** MAC por paquete y ventana anti-replay.
- **Cliente tramposo:** el servidor es autoritativo, el cliente solo envía intenciones, la velocidad la impone la simulación y un sistema de créditos limita los inputs por tick; los mensajes malformados llevan a expulsión.
- **Inundación:** límite de paquetes por segundo y por sesión, de sesiones y de sesiones pendientes.
- **No cubierto aún:** confidencialidad (no hay cifrado), credenciales asimétricas, ataques volumétricos de red (eso se mitiga fuera del motor), trampas que no alteran el protocolo (por ejemplo, ayudas de puntería) y atestación del cliente.

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
