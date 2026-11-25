# Estado verificado y pendientes (versión 0.1.0)

Este documento separa lo que está probado de lo que no, para no confundir intención con realidad.

## Verificado con pruebas automáticas (64)
- **Grafo:** validación de conexiones y tipos, apagado selectivo, presupuesto y prioridades, flujo de datos, descarte de salidas inválidas, ejecución paralela idéntica a la secuencial, hot-swap (compatibilidad, estado, permisos), exportación DOT.
- **Mundo:** consultas idénticas a fuerza bruta, histéresis de interés, regiones activas, reparto en shards equilibrado, determinista y con movimiento mínimo al añadir shards.
- **Red:** ida y vuelta de snapshots completos y delta con cambios aleatorios (200 iteraciones), rechazo de basura, confirmaciones con pérdida, desorden, duplicados y vuelta del contador, reconciliación de predicción.
- **Módulos:** SHA-256 contra los vectores oficiales (incluido un millón de "a"), HMAC contra RFC 4231, manifiestos (formato estricto), detección de payload o manifiesto alterados, firmante desconocido, revocación, permisos prohibidos y permisos no declarados.
- **Herramienta:** pruebas de extremo a extremo del binario `tsofgw`: hash, pack, verify con payload alterado y clave equivocada, módulos sin firma, demo, dot, new. La plantilla de `tsofgw new` se compiló y ejecutó aparte.
- **Portabilidad:** todo pasa con la librería C y con el respaldo en Rust puro (`TSOFGW_NO_C=1`); `cargo fmt` y `cargo clippy` sin avisos.

## No verificado o no implementado
- **Windows y macOS:** la CI está escrita para Linux, Windows y macOS, pero aún no ha corrido en GitHub. Todo se probó en Linux.
- **Backends reales:** no hay render con wgpu ni OpenXR; la HAL tiene interfaces y backends headless.
- **Sandbox de módulos:** los permisos se declaran, se verifican al cargar y se auditan, pero no hay aislamiento en tiempo de ejecución (WebAssembly pendiente).
- **Firmas de distribución pública:** solo HMAC con clave compartida (útil para pruebas y redes privadas). Hacen falta firmas asimétricas (Ed25519) con una dependencia auditada; no se implementó criptografía asimétrica propia a propósito.
- **Red:** sin transporte ni cifrado; sin control de congestión.
- **Interoperabilidad de cuentas:** solo está en el diseño.
- **Consolas:** solo la política; los backends dependen de SDK bajo NDA.
- **Rendimiento:** las cifras de la demo son de un sandbox de 2 núcleos; faltan mediciones en hardware real, perfiles calibrados y costo medido frente al declarado.

## Corrección registrada
La primera versión del módulo en C (`hash.c`) tenía la constante inicial de FNV-1a con un dígito de menos, así que no producía FNV-1a real. Se detectó al añadir vectores de referencia públicos y se corrigió en 0.1.0. Ese hash es solo un identificador rápido y no criptográfico; para integridad y seguridad se usa SHA-256.

## Siguientes pasos sugeridos
1. Confirmar la CI en Windows y macOS.
2. Backend wgpu (triángulo en pantalla) y backend OpenXR.
3. Medir el costo real de cada nodo y compararlo con el declarado.
4. Sandbox WebAssembly y firmas Ed25519.
5. Transporte UDP con cifrado y entidades fantasma entre shards.
