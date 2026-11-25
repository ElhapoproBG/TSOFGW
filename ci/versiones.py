#!/usr/bin/env python3
"""Imprime las versiones de las herramientas de Rust que usa la integración continua.

Existe para que el paso «Versiones» sea idéntico en Linux, macOS y Windows. Antes se ejecutaba
`bash -c '...'` desde `ci/annotate.py`, y en Windows `bash` resuelve a `C:\\Windows\\System32\\bash.exe` (el
lanzador de WSL) y no a Git Bash: el trabajo moría a los 23 s con «no hay distribuciones instaladas» y no
llegaba a ejecutar ni una prueba. Aquí cada herramienta se llama directamente, sin intérprete de órdenes.

Código de salida: 0 si las tres herramientas respondieron; si alguna falla, el de la primera que falló.
"""

import subprocess
import sys

COMANDOS = (
    ["rustc", "--version", "--verbose"],
    ["cargo", "--version"],
    ["cargo", "clippy", "--version"],
)


def main():
    codigo = 0
    for comando in COMANDOS:
        try:
            r = subprocess.run(comando, capture_output=True, text=True, encoding="utf-8", errors="replace")
        except OSError as e:
            print(f"{' '.join(comando)}: {e}", flush=True)
            codigo = codigo or 127
            continue
        sys.stdout.write(r.stdout)
        sys.stdout.write(r.stderr)
        sys.stdout.flush()
        codigo = codigo or r.returncode
    return codigo


if __name__ == "__main__":
    sys.exit(main())
