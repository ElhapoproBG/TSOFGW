# Experiencia del desarrollador

TSOFGW no exige una aplicación propia para trabajar. Se ofrece por capas, de la más simple a la más ambiciosa, y cada capa es útil sin la siguiente.

## Capa 1: biblioteca (disponible desde H1)
El desarrollador crea su proyecto en Rust, añade `tsofgw-core` como dependencia y escribe sus nodos implementando el contrato (`Node`): puertos tipados, permisos, costo y estado.

```rust
let mut grafo = Graph::new();
let fisica = grafo.add(Box::new(MiFisica::new()), &[])?;
let render = grafo.add(
    Box::new(MiRender::new()),
    &[Link { from: fisica, output: "pos", input: "pos" }],
)?;
grafo.set_demanded(render, true);
loop { grafo.tick(); }
```

Referencia: el modelo de Bevy, que funciona como biblioteca sin editor.

## Capa 2: herramienta de línea de comandos `tsofgw` (objetivo: H2)
- `tsofgw new`: crea un proyecto con la estructura estándar.
- `tsofgw run` y `tsofgw build`: ejecutar y compilar, con perfiles `vr`, `pc` y `consola`.
- `tsofgw pack` y `tsofgw sign`: empaquetar y firmar módulos.
- `tsofgw add`: instalar módulos oficiales o certificados, verificando firma y permisos (ver documento 03).

## Capa 3: inspector del grafo (objetivo: H3)
Vista en vivo del grafo: nodos activos e hibernados, costos, permisos, conexiones y errores de contrato. Es una herramienta necesaria, no un lujo: depurar un grafo grande sin ella es impracticable.

## Capa 4: editor visual (solo cuando haya demanda)
Arrastrar nodos y conectar puertos. El grafo tipado de TSOFGW ya contiene todo lo que un editor necesita, así que esta capa se construye sobre las anteriores sin rediseñar el motor.

## Lenguajes
- **Rust:** primera clase, para el núcleo y para nodos.
- **C:** apoyo vía FFI encapsulado (`tsofgw-sys`).
- **Otros lenguajes (C#, C++, Go, etc.):** nodos de alto nivel compilados a WebAssembly y ejecutados en sandbox con los permisos que declaren. Es la vía principal para bajar la barrera de entrada al mercado MMO.

## Principios
1. Útil pronto: biblioteca primero, herramientas después.
2. No bloquear al desarrollador: todo lo que hace el editor se puede hacer en código.
3. Seguridad por defecto: firmas y permisos están en la herramienta desde la capa 2.
