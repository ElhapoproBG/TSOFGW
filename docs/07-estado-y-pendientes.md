# Estado verificado y pendientes (versión 0.4.0)

Este documento separa lo que está probado de lo que no, para no confundir intención con realidad.

## Verificado con pruebas automáticas
- **Grafo:** validación de conexiones y tipos, apagado selectivo, presupuesto y prioridades, flujo de datos, descarte de salidas inválidas, ejecución paralela idéntica a la secuencial, hot-swap (compatibilidad, estado, permisos), exportación DOT.
- **Mundo:** consultas idénticas a fuerza bruta, histéresis de interés, regiones activas, reparto en shards equilibrado, determinista y con movimiento mínimo al añadir shards.
- **Red:** ida y vuelta de snapshots completos y delta con cambios aleatorios (200 iteraciones), rechazo de basura, confirmaciones con pérdida, desorden, duplicados y vuelta del contador, reconciliación de predicción.
- **Módulos:** SHA-256 contra los vectores oficiales (incluido un millón de "a"), HMAC contra RFC 4231, manifiestos (formato estricto), detección de payload o manifiesto alterados, firmante desconocido, revocación, permisos prohibidos y permisos no declarados.
- **Herramienta:** pruebas de extremo a extremo del binario `tsofgw`: hash, pack, verify con payload alterado y clave equivocada, módulos sin firma, demo, dot, new. La plantilla de `tsofgw new` se compiló y ejecutó aparte.
- **Render por software (0.2.0):** matemática de matrices y cuaterniones, recorte contra el plano cercano, z-buffer, colores con corrección de perspectiva, escena estéreo con fov asimétrico, codificador PNG (validado con PIL), comparación de imágenes y `tsofgw render` de extremo a extremo. Las imágenes de `docs/img` salen de este rasterizador.
- **Red en línea (0.3.0):** handshake mutuo con cookie, rechazo de clave equivocada, de paquetes alterados, repetidos y de otra dirección; sin amplificación; fragmentación con pérdida, duplicados y desorden; handshake bajo 35 % de pérdida en 30 semillas; 200 clientes simultáneos; timeouts; reemplazo de sesión; límite de paquetes. Servidor autoritativo: convergencia exacta de predicción y de vistas delta con 5 % y 30 % de pérdida, clientes que inundan de inputs o envían direcciones exageradas, expulsión por basura, apagado selectivo (≈ 25 % de NPC simulados, 0 sin jugadores), escenarios reproducibles. Servidor y bots reales como procesos separados por UDP.
- **Escala dentro de un servidor (0.4.0):** la huella de *todos* los paquetes de la red es idéntica con 1, 2, 3 y 8 hilos (también con LOD y presupuesto activos); compartir la vecindad entre jugadores de una celda no cambia ni un byte; el LOD ahorra ancho de banda sin desincronizar al cliente (su vista reconstruida coincide con lo registrado) y con el mundo quieto acaba igual que sin LOD; el presupuesto respeta el tope por snapshot y su recorte, calculado sin codificar, coincide con el de la versión lenta de referencia en 400 rondas aleatorias × 11 presupuestos.
- **Clúster de shards (0.4.0):** reparto 2×2 con jugadores que cruzan bordes una y otra vez: ningún NPC ni jugador se pierde o duplica (durante el juego nunca se pierde nada; un duplicado solo existe esperando un acuse), cada traspaso es exactamente un cambio de shard, 0 correcciones de predicción y ningún salto mayor a un paso normal; lo que ve un jugador junto al borde coincide con la verdad del clúster; resiste pérdida de paquetes de jugadores, un bus con 25 % de pérdida y 25 % de duplicados y otro con 50 %, 40 % y 120 ms de jitter; un bus caído cancela los traspasos sin perder jugadores y se recupera; mensajes hostiles; un solo shard equivale a un servidor suelto (misma huella de paquetes); determinismo; shards con numeración de ticks distinta; configuraciones incompatibles se rechazan.
- **Bus UDP real (0.4.0):** formato, etiqueta HMAC (un bit alterado en cualquier byte se detecta), ventana anti-replay, reinicio con capturas viejas, relojes desfasados (±30 s), resumen de configuración distinto, inundación y destinos imposibles; un clúster 2×2 sobre sockets reales (42 traspasos, 0 rechazos), un intruso que forja datagramas (todos rechazados y registrados) y una clave de clúster distinta (no se fían y no se pierde nada); procesos `serve --shard` reales con bots que patrullan.
- **Optimizaciones del tick:** cada una comparada con la versión simple (consultas contra fuerza bruta, interés contra referencia durante 300 ticks, `encode_delta` byte a byte, movimiento sobre las 16,7 millones de direcciones).
- **Portabilidad:** todo pasa con la librería C y con el respaldo en Rust puro (`TSOFGW_NO_C=1`); `cargo fmt` y `cargo clippy` sin avisos.

## No verificado o no implementado
- **Windows y macOS:** la CI está escrita para Linux, Windows y macOS, pero aún no ha corrido en GitHub. Todo se probó en Linux.
- **Backend wgpu (`gpu/tsofgw-wgpu`):** escrito siguiendo la documentación de wgpu 30.0.2, pero **no se ha compilado ni ejecutado con el wgpu real** (el entorno donde se escribió no podía descargar paquetes). Solo se comprobó que su lógica, tipos y préstamos son correctos contra una maqueta de la API que escribí yo, lo que no demuestra que los nombres coincidan con la biblioteca real. Puntos con más riesgo: `PollType::wait()`, el tipo de retorno de `request_adapter` y `DeviceDescriptor::default()`. La CI incluye un trabajo que lo compila y compara su imagen con la del rasterizador por software.
- **OpenXR:** no empezado; la HAL tiene la interfaz y un backend headless.
- **Ventana y presentación en pantalla:** no hay; el render actual es fuera de pantalla.
- **Sandbox de módulos:** los permisos se declaran, se verifican al cargar y se auditan, pero no hay aislamiento en tiempo de ejecución (WebAssembly pendiente).
- **Firmas de distribución pública:** solo HMAC con clave compartida (útil para pruebas y redes privadas). Hacen falta firmas asimétricas (Ed25519) con una dependencia auditada; no se implementó criptografía asimétrica propia a propósito.
- **Red:** hay transporte autenticado pero **sin cifrado** (ni en el transporte de jugadores ni en el bus entre shards); credenciales por clave compartida (sin Ed25519); sin migración de dirección ni control de congestión; la ruta de Windows para obtener aleatoriedad (`BCryptGenRandom`) no se pudo compilar aquí. Medido solo en simulación y bucle local.
- **Clúster:** sin persistencia (un shard reiniciado pierde su estado; un traspaso perdido durante una partición larga puede perder a un jugador); reparto estático en rejilla, sin equilibrado dinámico; sin directorio de sesiones entre shards (una cuenta podría estar en dos shards a la vez); quien tenga la clave de clúster puede suplantar a cualquier shard; el bus necesita relojes sincronizados (±30 s); probado en una sola máquina, no en varias ni con latencias de internet; sin pruebas de apagar un shard en caliente.
- **Escala del servidor:** con 2 hilos en 2 núcleos el tick medio con 500 jugadores densos baja de 22,7 ms a 14,8 ms (1,5×); el escalado con más núcleos (y en hardware real) no está medido. La simulación de NPC sigue en un solo hilo.
- **Interoperabilidad de cuentas:** solo está en el diseño.
- **Consolas:** solo la política; los backends dependen de SDK bajo NDA.
- **Rendimiento:** las cifras de la demo son de un sandbox de 2 núcleos; faltan mediciones en hardware real, perfiles calibrados y costo medido frente al declarado.

## Corrección registrada
La primera versión del módulo en C (`hash.c`) tenía la constante inicial de FNV-1a con un dígito de menos, así que no producía FNV-1a real. Se detectó al añadir vectores de referencia públicos y se corrigió en 0.1.0. Ese hash es solo un identificador rápido y no criptográfico; para integridad y seguridad se usa SHA-256.

**0.4.0.** Un cliente que cambiaba a un shard cuya cuenta de ticks iba por detrás descartaba todos sus snapshots y el mundo se quedaba quieto (documento 09, sección 8). Lo encontró una prueba endurecida a propósito, no la primera versión. Además, dos optimizaciones del propio 0.4.0 empezaron costando más CPU de la que ahorraban en red (el LOD con una búsqueda por entidad y el presupuesto recodificando cada candidato); las mediciones lo mostraron y se corrigieron con un recorrido en paralelo de dos listas ordenadas y con el cálculo exacto de tamaños (documento 08).

## Siguientes pasos sugeridos
1. Confirmar la CI en Windows y macOS.
2. Compilar `gpu/tsofgw-wgpu` con el wgpu real (ver `gpu/README.md`), corregir lo que falle, y añadir ventana; después backend OpenXR.
3. Medir el costo real de cada nodo y compararlo con el declarado.
4. Sandbox WebAssembly y firmas Ed25519.
5. Cifrado AEAD del transporte y del bus (biblioteca auditada) y credenciales Ed25519.
6. Capa de persistencia (estado de jugadores y NPC por shard, recuperación tras reinicio y traspasos a prueba de particiones largas) y directorio de sesiones entre shards.
7. Probar el clúster en varias máquinas reales: apagar un shard en caliente, latencias de WAN, relojes desfasados.
8. Simulación de NPC en paralelo y equilibrado dinámico de regiones.
