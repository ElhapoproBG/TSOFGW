# Arquitectura

## Capas
1. **Hardware (HAL):** wgpu/Vulkan/Metal/D3D12, OpenXR, audio, entrada. Rust; librerías C vía `tsofgw-sys`.
2. **Grafo (núcleo):** nodos tipados, dependencias explícitas, planificador, estados de nodo.
3. **Nodos estándar:** simulación (ECS), red, persistencia, cuentas, render, audio.
4. **Juego:** nodos y datos del desarrollador.

## Estados de nodo
`Active` (corre) · `Hibernated` (conserva estado, no corre) · `Unloaded` (pendiente).
Un nodo es Active si es demandado o si algún Active depende de él (cierre transitivo).

## Contrato de nodo (v0)
nombre, dependencias, capacidades requeridas (`net`, `disk`, `accounts`…), costo declarado (CPU/GPU/mem), `tick`.

## IR y compilación (siguiente fase)
El grafo se compila a planes de ejecución especializados; solo se recompila el subgrafo cambiado (hot-swap). Objetivo: 90–120 Hz estables en VR.

## Cliente/servidor
El mismo grafo se particiona: el planificador decide qué subgrafo corre en cada servidor y en el visor. El planificador debe ser determinista donde el netcode lo exija.

## Hitos
- H0 (hecho): grafo + apagado selectivo + FFI C.
- H1: contratos tipados, costos declarados, hot-swap de un nodo.
- H2: nodo wgpu (triángulo) + nodo de red.
- H3: MMO mínimo, 200–500 jugadores en mundo continuo con celdas espaciales e interest management.
- H4: módulos firmados, sandbox WASM, identidad federada.

## Plataformas (VR primero, sin dejar fuera PC y consolas)
Cada plataforma es un **backend de la capa HAL**, expuesto como nodos intercambiables: el código de juego no cambia, cambian los nodos de salida y entrada.

| Plataforma | Salida / entrada | Notas |
|---|---|---|
| VR (prioridad) | OpenXR, render estéreo | 90–120 Hz estables; presupuesto de latencia estricto |
| PC (Windows/Linux/macOS) | Vulkan / D3D12 / Metal vía wgpu; teclado, ratón, gamepad | Mismo grafo, render en pantalla plana |
| Consolas | SDK propietario de cada fabricante | Ver abajo |

**Perfiles de ejecución:** un perfil (`vr`, `pc`, `consola`) fija presupuestos de frame, calidad y qué nodos se hibernan. El apagado selectivo ayuda aquí: en consola y en visores autónomos se apagan los nodos que no caben en el presupuesto.

**Consolas y código abierto:** los SDK de consola (PlayStation, Xbox, Nintendo) están bajo NDA y no pueden publicarse. Por eso el núcleo y los contratos de backend son abiertos, y los backends de consola los desarrollan y mantienen en privado quienes tengan licencia de desarrollador de cada fabricante, usando la misma interfaz HAL. Apache-2.0 lo permite. Los backends de consola privados seguirán la política de firmas y certificación del documento 04.

**Hitos:** H2 añade backend PC (wgpu) y OpenXR; la interfaz HAL se congela antes de H3 para que terceros puedan escribir backends de consola.
