# Mundo y red

## Mundo (`tsofgw-world`)

**Rejilla espacial.** El mundo se divide en celdas cúbicas del mismo tamaño. Cada entidad vive en una celda; las celdas vacías se eliminan, así que un mundo enorme y disperso no cuesta memoria. Las consultas por radio visitan solo las celdas que toca la esfera (o recorren las celdas ocupadas si el radio es enorme) y devuelven resultados ordenados por id, de forma determinista. Hay una prueba que compara las consultas con fuerza bruta.

**Gestión de interés.** Cada observador recibe solo lo que está dentro de su radio. Para evitar que una entidad en el borde entre y salga en cada tick, se usa histéresis: algo que ya se veía sigue visible hasta `radio × histéresis`, y para volver a entrar debe estar dentro del radio estricto. `Interest::update` devuelve, por observador, qué entidades entraron y cuáles salieron.

**Regiones activas.** `active_cells` devuelve las celdas ocupadas que tocan alguna esfera de observador; solo las entidades de esas celdas se simulan. Es la versión del apagado selectivo aplicada al mundo: donde no hay nadie cerca, no se gasta CPU.

**Shards.** Las regiones (bloques de celdas) se reparten entre servidores con *rendezvous hashing*: cada región va al shard con mayor puntuación. Ventaja medida en las pruebas: al añadir un quinto shard a cuatro, se mueve cerca de 1/5 de las regiones (no más), y todas las que se mueven van al shard nuevo; quitarlo devuelve el reparto original. `ShardMap::migration` indica cuándo un cambio de celda cruza a otro shard y hay que migrar la entidad.

## Red (`tsofgw-net`)

Independiente del transporte (UDP, WebTransport…): trabaja con bytes.

- **Codificación:** varints y zigzag, con errores de decodificación explícitos (truncado, desbordamiento, basura al final). Las posiciones se cuantizan a 1/64 de unidad.
- **Snapshots:** completos o **delta** contra una línea base que el cliente ya confirmó. El delta codifica solo altas, bajas y los componentes que cambiaron, con ids codificados por diferencias. Si la línea base no coincide o hay una referencia inexistente, la decodificación falla con un error claro en vez de producir estado corrupto.
- **Confirmaciones:** cabecera de 8 bytes con número de secuencia, último recibido y máscara de los 32 anteriores. Soporta pérdida, desorden, duplicados y vuelta del contador de 16 bits.
- **Predicción:** `Predictor` aplica los inputs del jugador de inmediato y, al llegar el estado autoritativo, descarta lo ya procesado y reaplica lo pendiente. Requiere una simulación determinista compartida entre cliente y servidor.

## Cifras de la demo (`tsofgw demo`)
Escenario: 50 000 entidades en un mundo de 10 km × 10 km, 100 jugadores con radio de interés de 150, en un sandbox de 2 núcleos (hilo único):

- Se simula alrededor del 21 % de las entidades por tick (las que están cerca de algún jugador).
- Un jugador ve unas 35 entidades; el snapshot delta pesa alrededor del 56 % del completo **en el peor caso, donde cada entidad simulada se mueve en cada tick**. Con entidades quietas el ahorro es mucho mayor (en las pruebas, con 50 de 1000 entidades en movimiento, el delta pesa menos de la décima parte).
- Tiempo real: unos 7 ms por tick en esa máquina.

Son cifras de un entorno concreto: sirven para comparar perfiles y detectar regresiones, no como promesa de rendimiento en producción.

## Límites conocidos
- No hay entidades fantasma en las fronteras de shard: un jugador cerca del borde todavía no ve lo que está al otro lado si está en otro servidor.
- El transporte real está en el documento 08 (UDP autenticado, sin cifrado todavía); este documento describe los bloques de codificación.
- Las posiciones se cuantizan con un rango de ±33 millones de unidades por eje.
- El costo declarado de los nodos es una declaración, no una medición. En la demo con 100 000 entidades y 200 jugadores, el tiempo real supera el presupuesto del perfil de consola aunque el costo declarado quepa. La medición del costo real y su comparación con el declarado es una tarea pendiente.
