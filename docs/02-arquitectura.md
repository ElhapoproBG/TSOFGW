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
