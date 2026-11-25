#!/usr/bin/env python3
"""Ejecuta un comando, deja su salida en el registro y publica el resultado como anotaciones de GitHub.

    python ci/annotate.py TÍTULO [--json] [--notice-tail N] [--notice-grep REGEX] -- COMANDO...

Por qué existe: GitHub solo enseña el texto de los registros a quien ha iniciado sesión, pero las anotaciones
de una ejecución son públicas (`GET /repos/<dueño>/<repo>/check-runs/<id>/annotations`). Publicando ahí los
errores de formato, de Clippy, de compilación y de pruebas, cualquier persona (y cualquier herramienta) puede
leerlos sin credenciales y sin abrir el registro.

* Si el comando falla: una anotación de error con todo lo relevante (diagnósticos del compilador, líneas de
  pánico y de aserción, final de la salida) y, con `--json`, una anotación con archivo y línea por diagnóstico.
* Si el comando tiene éxito: nada, salvo que se pida un aviso (`notice`) con `--notice-tail N` (las últimas N
  líneas) o `--notice-grep REGEX` (las líneas que casen). Sirve para ver cifras de las pruebas de humo.

Con `--json` el comando debe empezar por `cargo <subcomando>`: se le añade `--message-format=json`.
El código de salida es el del comando. Solo usa la biblioteca estándar de Python.
"""

import argparse
import collections
import json
import os
import re
import subprocess
import sys

MAX_INLINE = 8  # anotaciones con archivo y línea (GitHub admite 10 por nivel y paso)
MAX_SUMMARY = 28_000  # caracteres del resumen (GitHub admite hasta 64 KB por anotación)
MAX_INLINE_TEXT = 4_000
TAIL_LINES = 60
MAX_INTERESTING = 120
MAX_NOTICE_LINES = 60

INTERESTING = re.compile(
    r"panicked at|FAILED|\bfailed\b|^error(\[|:)|^warning:|assertion|left:|right:|^---- |^failures:|"
    r"test result:|Diff in|OMITIDA|DIFIEREN|COINCIDEN|adaptador|Caused by:|could not|failed to|"
    r"no se pudo|invariante|perdid|duplicad",
    re.IGNORECASE,
)


def _data(texto):
    """Escapa el texto de un comando de flujo de trabajo."""
    return texto.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def _prop(texto):
    """Escapa el valor de una propiedad de un comando de flujo de trabajo."""
    return _data(texto).replace(":", "%3A").replace(",", "%2C")


def emitir(nivel, titulo, mensaje, archivo=None, linea=None, col=None, fin=None):
    props = []
    if archivo:
        props.append(f"file={_prop(archivo)}")
    if linea:
        props.append(f"line={linea}")
    if fin:
        props.append(f"endLine={fin}")
    if col:
        props.append(f"col={col}")
    props.append(f"title={_prop(titulo)}")
    print(f"::{nivel} {','.join(props)}::{_data(mensaje)}", flush=True)


def recortar(texto, maximo):
    if len(texto) <= maximo:
        return texto
    return texto[: maximo - 20].rstrip() + "\n…(recortado)"


class Diagnostico:
    def __init__(self, nivel, codigo, texto, archivo, linea, col, fin, dibujado):
        self.nivel, self.codigo, self.texto = nivel, codigo, texto
        self.archivo, self.linea, self.col, self.fin = archivo, linea, col, fin
        self.dibujado = dibujado

    def clave(self):
        return (self.nivel, self.codigo, self.texto, self.archivo, self.linea, self.col)


def leer_diagnostico(msg):
    """Convierte un mensaje del compilador (JSON de cargo) en un diagnóstico, o None si no interesa."""
    nivel = msg.get("level", "")
    if nivel != "warning" and not nivel.startswith("error"):
        return None
    spans = msg.get("spans") or []
    principal = next((s for s in spans if s.get("is_primary")), spans[0] if spans else None)
    codigo = (msg.get("code") or {}).get("code")
    texto = msg.get("message", "")
    if principal is None and codigo is None:
        return None  # resúmenes como «aborting due to 2 previous errors»
    archivo = linea = col = fin = None
    if principal is not None:
        nombre = principal.get("file_name", "").replace("\\", "/")
        # Solo rutas relativas al repositorio: las de la biblioteca estándar o de dependencias no sirven.
        if nombre and not nombre.startswith("/") and ":" not in nombre:
            archivo = nombre
            linea = principal.get("line_start")
            col = principal.get("column_start")
            fin = principal.get("line_end")
    dibujado = (msg.get("rendered") or texto).rstrip()
    return Diagnostico("warning" if nivel == "warning" else "error", codigo, texto, archivo, linea, col, fin, dibujado)


def resumen_texto_plano(lineas, cola):
    """Líneas relevantes de la salida (con dos de contexto) y el final del registro."""
    relevantes, vistos = [], set()
    for i, linea in enumerate(lineas):
        if INTERESTING.search(linea):
            for j in range(i, min(i + 3, len(lineas))):
                if j not in vistos:
                    vistos.add(j)
                    relevantes.append(lineas[j])
        if len(relevantes) >= MAX_INTERESTING:
            break
    partes = []
    if relevantes:
        partes.append("Líneas relevantes:\n" + "\n".join(relevantes))
    partes.append("Final de la salida:\n" + "\n".join(cola))
    return "\n\n".join(partes)


def informar_fallo(titulo, codigo, diagnosticos, lineas, cola):
    partes = []
    if diagnosticos:
        errores = sum(1 for d in diagnosticos if d.nivel == "error")
        avisos = len(diagnosticos) - errores
        partes.append(f"{errores} errores y {avisos} avisos del compilador:")
        for i, d in enumerate(diagnosticos, 1):
            partes.append(f"\n#{i} {d.dibujado}")
    partes.append(resumen_texto_plano(lineas, cola))
    resumen = recortar("\n".join(partes), MAX_SUMMARY)

    con_sitio = [d for d in diagnosticos if d.archivo]
    for d in con_sitio[:MAX_INLINE]:
        emitir(
            "error" if d.nivel == "error" else "warning",
            f"{titulo}: {d.codigo or d.nivel}",
            recortar(d.dibujado, MAX_INLINE_TEXT),
            d.archivo,
            d.linea,
            d.col,
            d.fin,
        )
    emitir("error", f"{titulo} falló (código {codigo})", resumen)

    ruta = os.environ.get("GITHUB_STEP_SUMMARY")
    if ruta:
        with open(ruta, "a", encoding="utf-8") as f:
            f.write(f"### {titulo} falló (código {codigo})\n\n```text\n{recortar(resumen, 20_000)}\n```\n")


def main():
    argv = sys.argv[1:]
    if "--" not in argv:
        sys.exit("uso: annotate.py TÍTULO [--json] [--notice-tail N] [--notice-grep REGEX] -- COMANDO...")
    corte = argv.index("--")
    cabeza, comando = argv[:corte], argv[corte + 1 :]
    p = argparse.ArgumentParser(prog="annotate.py")
    p.add_argument("titulo")
    p.add_argument("--json", action="store_true")
    p.add_argument("--notice-tail", type=int, default=0)
    p.add_argument("--notice-grep", default=None)
    args = p.parse_args(cabeza)
    if not comando:
        sys.exit("falta el comando después de --")
    if args.json:
        if len(comando) < 2 or comando[0] != "cargo":
            sys.exit("--json exige un comando `cargo <subcomando> ...`")
        comando = [comando[0], comando[1], "--message-format=json"] + comando[2:]

    for flujo in (sys.stdout, sys.stderr):
        try:
            flujo.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass

    print(f"[annotate] {args.titulo}: {' '.join(comando)}", flush=True)
    try:
        proc = subprocess.Popen(
            comando,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
    except OSError as e:
        emitir("error", f"{args.titulo} no pudo ejecutarse", f"{comando[0]}: {e}")
        return 127

    lineas = []
    cola = collections.deque(maxlen=TAIL_LINES)
    diagnosticos, vistos = [], set()
    for bruta in proc.stdout:
        linea = bruta.rstrip("\r\n")
        if args.json and linea.startswith("{"):
            try:
                obj = json.loads(linea)
            except ValueError:
                obj = None
            if isinstance(obj, dict) and "reason" in obj:
                if obj["reason"] == "compiler-message":
                    d = leer_diagnostico(obj.get("message") or {})
                    if d is not None:
                        print(d.dibujado, flush=True)
                        if d.clave() not in vistos:  # cargo repite los de lib y lib-test
                            vistos.add(d.clave())
                            diagnosticos.append(d)
                continue
        print(linea, flush=True)
        lineas.append(linea)
        cola.append(linea)
    codigo = proc.wait()
    print(f"[annotate] {args.titulo}: código de salida {codigo}", flush=True)

    if codigo != 0:
        informar_fallo(args.titulo, codigo, diagnosticos, lineas, list(cola))
    elif args.notice_grep or args.notice_tail:
        elegidas = []
        if args.notice_grep:
            patron = re.compile(args.notice_grep)
            elegidas = [l for l in lineas if patron.search(l)]
        if args.notice_tail:
            elegidas += list(cola)[-args.notice_tail :]
        if elegidas:
            emitir("notice", f"{args.titulo}: resumen", "\n".join(elegidas[-MAX_NOTICE_LINES:]))
    return codigo


if __name__ == "__main__":
    sys.exit(main())
