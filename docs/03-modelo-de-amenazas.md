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
