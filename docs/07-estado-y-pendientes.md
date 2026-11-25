# Estado verificado y pendientes (versión 0.3.0)

Este documento separa lo que está probado de lo que no, para no confundir intención con realidad.

## Verificado con pruebas automáticas
- **Grafo:** validación de conexiones y tipos, apagado selectivo, presupuesto y prioridades, flujo de datos, descarte de salidas inválidas, ejecución paralela idéntica a la secuencial, hot-swap (compatibilidad, estado, permisos), exportación DOT.
- **Mundo:** consultas idénticas a fuerza bruta, histéresis de interés, regiones activas, reparto en shards equilibrado, determinista y con movimiento mínimo al añadir shards.
- **Red:** ida y vuelta de snapshots completos y delta con cambios aleatorios (200 iteraciones), rechazo de basura, confirmaciones con pérdida, desorden, duplicados y vuelta del contador, reconciliación de predicción.
- **Módulos:** SHA-256 contra los vectores oficiales (incluido un millón de "a"), HMAC contra RFC 4231, manifiestos (formato estricto), detección de payload o manifiesto alterados, firmante desconocido, revocación, permisos prohibidos y permisos no declarados.
- **Herramienta:** pruebas de extremo a extremo del binario `tsofgw`: hash, pack, verify con payload alterado y clave equivocada, módulos sin firma, demo, dot, new. La plantilla de `tsofgw new` se compiló y ejecutó aparte.
- **Render por software (0.2.0):** matemática de matrices y cuaterniones, recorte contra el plano cercano, z-buffer, colores con corrección de perspectiva, escena estéreo con fov asimétrico, codificador PNG (validado con PIL), comparación de imágenes y `tsofgw render` de extremo a extremo. Las imágenes de `docs/img` salen de este rasterizador.
- **Red en línea (0.3.0):** handshake mutuo con cookie, rechazo de clave equivocada, de paquetes alterados, repetidos y de otra dirección; sin amplificación; fragmentación con pérdida, duplicados y desorden; handshake bajo 35 % de pérdida en 30 semillas; 200 clientes simultáneos; timeouts; reemplazo de sesión; límite de paquetes. Servidor autoritativo: convergencia exacta de predicción y de vistas delta con 5 % y 30 % de pérdida, clientes que inundan de inputs o envían direcciones exageradas, expulsión por basura, apagado selectivo (≈ 25 % de NPC simulados, 0 sin jugadores), escenarios reproducibles. Servidor y bots reales como procesos separados por UDP.
- **Optimizaciones del tick:** cada una comparada con la versión simple (consultas contra fuerza bruta, interés contra referencia durante 300 ticks, `encode_delta` byte a byte, movimiento sobre las 16,7 millones de direcciones).
- **Portabilidad:** todo pasa con la librería C y con el respaldo en Rust puro (`TSOFGW_NO_C=1`); `cargo fmt` y `cargo clippy` sin avisos.

## No verificado o no implementado
- **Windows y macOS:** la CI está escrita para Linux, Windows y macOS, pero aún no ha corrido en GitHub. Todo se probó en Linux.
- **Backend wgpu (`gpu/tsofgw-wgpu`):** escrito siguiendo la documentación de wgpu 30.0.2, pero **no se ha compilado ni ejecutado con el wgpu real** (el entorno donde se escribió no podía descargar paquetes). Solo se comprobó que su lógica, tipos y préstamos son correctos contra una maqueta de la API que escribí yo, lo que no demuestra que los nombres coincidan con la biblioteca real. Puntos con más riesgo: `PollType::wait()`, el tipo de retorno de `request_adapter` y `DeviceDescriptor::default()`. La CI incluye un trabajo que lo compila y compara su imagen con la del rasterizador por software.
- **OpenXR:** no empezado; la HAL tiene la interfaz y un backend headless.
- **Ventana y presentación en pantalla:** no hay; el render actual es fuera de pantalla.
- **Sandbox de módulos:** los permisos se declaran, se verifican al cargar y se auditan, pero no hay aislamiento en tiempo de ejecución (WebAssembly pendiente).
- **Firmas de distribución pública:** solo HMAC con clave compartida (útil para pruebas y redes privadas). Hacen falta firmas asimétricas (Ed25519) con una dependencia auditada; no se implementó criptografía asimétrica propia a propósito.
- **Red:** hay transporte autenticado pero **sin cifrado**; credenciales por clave compartida (sin Ed25519); sin migración de dirección ni control de congestión; la ruta de Windows para obtener aleatoriedad (`BCryptGenRandom`) no se pudo compilar aquí. Medido solo en simulación y loopback.
- **Escala del servidor:** un solo hilo; con 500 jugadores densos el tick máximo supera los 50 ms en el entorno de pruebas. Sin multihilo ni entidades fantasma entre shards.
- **Interoperabilidad de cuentas:** solo está en el diseño.
- **Consolas:** solo la política; los backends dependen de SDK bajo NDA.
- **Rendimiento:** las cifras de la demo son de un sandbox de 2 núcleos; faltan mediciones en hardware real, perfiles calibrados y costo medido frente al declarado.

## Corrección registrada
La primera versión del módulo en C (`hash.c`) tenía la constante inicial de FNV-1a con un dígito de menos, así que no producía FNV-1a real. Se detectó al añadir vectores de referencia públicos y se corrigió en 0.1.0. Ese hash es solo un identificador rápido y no criptográfico; para integridad y seguridad se usa SHA-256.

## Siguientes pasos sugeridos
1. Confirmar la CI en Windows y macOS.
2. Compilar `gpu/tsofgw-wgpu` con el wgpu real (ver `gpu/README.md`), corregir lo que falle, y añadir ventana; después backend OpenXR.
3. Medir el costo real de cada nodo y compararlo con el declarado.
4. Sandbox WebAssembly y firmas Ed25519.
5. Cifrado AEAD del transporte (biblioteca auditada) y credenciales Ed25519.
6. Multihilo del tick, entidades fantasma entre shards y menor frecuencia para entidades lejanas.
