#!/usr/bin/env python3
"""Prueba de las pruebas: ¿detectan de verdad los defectos que dicen detectar?

Que una suite pase no dice nada sobre su fuerza. Este script estropea el código a propósito, **de uno en
uno**, con defectos que un programador podría cometer de verdad (olvidar escribir a disco antes de acusar,
responder desde un directorio cuyo disco falló, no comprobar una etiqueta...) y ejecuta las pruebas. Un
defecto que ninguna prueba nota (un «mutante superviviente») es un hueco en la suite, y el script falla.

Qué hace, en este orden:
  1. copia el repositorio a una carpeta temporal (el árbol de trabajo no se toca nunca);
  2. comprueba que sin defectos las pruebas pasan (si no, no tendría sentido seguir);
  3. para cada mutante aplica el cambio, ejecuta las pruebas que le corresponden y clasifica:
       DETECTADO     compila y alguna prueba falla (lo que se quiere);
       SUPERVIVIENTE compila y todo pasa: la suite no ve ese defecto;
       NO COMPILA    el parche está mal o el código cambió: hay que actualizar este catálogo.

Uso (desde la raíz del repositorio):
    python ci/mutaciones.py --check            # solo comprueba que cada parche aún se aplica (rápido, sin compilar)
    python ci/mutaciones.py --list             # lista los mutantes
    python ci/mutaciones.py                    # los ejecuta todos (decenas de minutos)
    python ci/mutaciones.py --only P1,P13,S5   # solo algunos
    python ci/mutaciones.py --cargo-args=--offline

Cada mutante sustituye un texto que debe aparecer **una sola vez** en su archivo: si el código se refactoriza
y el texto se mueve, `--check` lo dice antes de que alguien se fíe de un catálogo desactualizado.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

CL = "crates/tsofgw-online/src/server/cluster.rs"
DU = "crates/tsofgw-online/src/server/durable.rs"
DI = "crates/tsofgw-online/src/directory.rs"
ST = "crates/tsofgw-store/src/lib.rs"
AE = "crates/tsofgw-aead/src/lib.rs"
WI = "crates/tsofgw-transport/src/wire.rs"
CR = "crates/tsofgw-transport/src/crypto.rs"
TS = "crates/tsofgw-transport/src/server.rs"
UB = "crates/tsofgw-online/src/udp_bus.rs"

# Pruebas que ve cada familia de mutantes.
PRUEBAS_ONLINE = ["-p", "tsofgw-online", "--test", "persist"]
PRUEBAS_STORE = ["-p", "tsofgw-store"]
PRUEBAS_AEAD = ["-p", "tsofgw-aead"]
PRUEBAS_TRANSPORT = ["-p", "tsofgw-transport"]
PRUEBAS_BUS = ["-p", "tsofgw-online", "--lib", "udp_bus"]


@dataclass
class Mutante:
    id: str
    defecto: str  # el error de programación que modela
    archivo: str
    viejo: str
    nuevo: str
    pruebas: list


MUTANTES = [
    # ------------------------------------------------------------------ el almacén
    Mutante(
        "S1", "el almacén acepta registros con la secuencia fuera de orden (diario con huecos o repetido)",
        ST,
        "            if seq != self.seq + 1 {",
        "            if false && seq != self.seq + 1 {",
        PRUEBAS_STORE,
    ),
    Mutante(
        "S2", "el almacén no comprueba la etiqueta de un registro (un bit alterado pasaría)",
        ST,
        "            if !ct_eq(tag, &self.record_tag(len as u32, seq, payload)) {",
        "            if false && !ct_eq(tag, &self.record_tag(len as u32, seq, payload)) {",
        PRUEBAS_STORE,
    ),
    Mutante(
        "S3", "el almacén no comprueba la etiqueta de la cabecera (una longitud falsa se creería)",
        ST,
        "            if !ct_eq(htag, &self.header_tag(len as u32, seq)) {",
        "            if false && !ct_eq(htag, &self.header_tag(len as u32, seq)) {",
        PRUEBAS_STORE,
    ),
    Mutante(
        "S4", "el almacén no autentica el checkpoint (cualquiera con acceso al disco lo edita)",
        ST,
        "    if !ct_eq(tag, &snapshot_tag(mac, body)) {",
        "    if false && !ct_eq(tag, &snapshot_tag(mac, body)) {",
        PRUEBAS_STORE,
    ),
    Mutante(
        "S5", "commit() no sincroniza: lo «durable» no lo es",
        ST,
        "        if let Err(e) = self.medium.sync() {\n            self.poisoned = true;\n            return Err(e.into());\n        }\n        self.unsynced = 0;\n        self.stats.syncs += 1;",
        "        if let Err(e) = Ok::<(), std::io::Error>(()) {\n            self.poisoned = true;\n            return Err(e.into());\n        }\n        self.unsynced = 0;\n        self.stats.syncs += 1;",
        PRUEBAS_STORE,
    ),
    Mutante(
        "S6", "apply() no sincroniza aunque lo pendiente supere el límite",
        ST,
        "        if self.unsynced >= self.cfg.max_unsynced {",
        "        if false && self.unsynced >= self.cfg.max_unsynced {",
        PRUEBAS_STORE,
    ),
    Mutante(
        "S7", "un checkpoint no sincroniza antes el diario viejo (si no llega a instalarse, se pierde lo último)",
        ST,
        "        // Lo escrito antes tiene que estar en el diario viejo por si el checkpoint no se completa.\n        self.commit()?;",
        "        // Lo escrito antes tiene que estar en el diario viejo por si el checkpoint no se completa.",
        PRUEBAS_STORE,
    ),
    Mutante(
        "S8", "un daño en medio del diario se toma por una cola rota y se descarta en silencio",
        ST,
        "                    OpenMode::Recover => Err(StoreError::Corrupt {\n                        what,\n                        offset: off as u64,\n                    }),",
        "                    OpenMode::Recover => {\n                        let _ = what;\n                        Ok(off as u64)\n                    }",
        PRUEBAS_STORE,
    ),
    Mutante(
        "S9", "tras recuperar una cola rota no se corta el diario (lo nuevo se escribe detrás de basura)",
        ST,
        "        if good < loaded.log.len() as u64 {\n            s.medium.truncate_log(good)?;\n        }",
        "        if false && good < loaded.log.len() as u64 {\n            s.medium.truncate_log(good)?;\n        }",
        PRUEBAS_STORE,
    ),
    # ------------------------------------------------------------------ persistencia del cluster
    Mutante(
        "P1", "el destino acusa un traspaso sin esperar a que lo adoptado esté en disco",
        CL,
        "        self.peer_send_durable(from, PeerMsg::HandoverAck { xfer, player });\n    }\n\n    /// Pone al jugador de una sesión ya conectada",
        "        self.peer_send(from, &PeerMsg::HandoverAck { xfer, player });\n    }\n\n    /// Pone al jugador de una sesión ya conectada",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P2", "el origen entrega a un jugador sin anotar antes el traspaso en disco",
        CL,
        "            pe.saved_pos.remove(&account);\n            pe.apply_critical(&b);\n        }\n        self.send_outbox_msg(account);",
        "            pe.saved_pos.remove(&account);\n            let _ = &b;\n        }\n        self.send_outbox_msg(account);",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P3", "el directorio responde sin haber sincronizado su disco",
        DI,
        "        if self.critical && !self.failed {\n            match self.store.commit() {",
        "        if false && self.critical && !self.failed {\n            match self.store.commit() {",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P4", "el directorio sigue respondiendo cuando su disco ya falló",
        DI,
        "        if self.failed {\n            self.stats.replies_dropped += deferred.len() as u64;\n            return;\n        }",
        "        if false && self.failed {\n            self.stats.replies_dropped += deferred.len() as u64;\n            return;\n        }",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P5", "un shard olvida qué época tenía cada cuenta (acepta traspasos viejos como nuevos)",
        DU,
        "        p.max(t).max(h)\n    }",
        "        let _ = (p, t, h);\n        0\n    }",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P6", "con persistencia, un traspaso sin acuse se abandona por tiempo (y la cuenta se pierde)",
        CL,
        "            if !persistent && age > timeout {\n                give_up.push(a);",
        "            if age > timeout {\n                give_up.push(a);",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P7", "al reiniciar no se restauran los traspasos de jugador sin acuse",
        DU,
        "            c.restore_outbox(account, r, tick);\n            report.handovers += 1;",
        "            let _ = (&c, &r, tick);\n            report.handovers += 1;",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P8", "un traspaso con la misma época que ya se conoce se adopta de nuevo",
        CL,
        "        if epoch <= self.known_epoch(player) {",
        "        if epoch < self.known_epoch(player) {",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P9", "un traspaso repetido o viejo (época ya conocida) se adopta y hace retroceder a la cuenta",
        CL,
        "        if epoch <= self.known_epoch(player) {",
        "        if false && epoch <= self.known_epoch(player) {",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P10", "el origen no deja lápida al cerrar un traspaso (una repetición tardía resucitaría la cuenta)",
        CL,
        "            b.delete(k_ho(account));\n            b.put(\n                k_tomb(account),\n                TombRec {\n                    epoch: ho.epoch,\n                    to: ho.to,\n                }\n                .encode(),\n            );\n            pe.apply(&b);\n        }\n        // Si el cliente sigue conectado aquí con el jugador congelado",
        "            b.delete(k_ho(account));\n            pe.apply(&b);\n        }\n        // Si el cliente sigue conectado aquí con el jugador congelado",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P11", "sin directorio, cualquier shard crea cuentas nuevas (una cuenta acaba en dos shards)",
        DU,
        "                (None, _) if me.is_some_and(|m| m != home) => Login::Redirect(home),",
        "                (None, _) if false && me.is_some_and(|m| m != home) => Login::Redirect(home),",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P12", "un shard cuyo disco falló sigue haciendo promesas que no puede cumplir",
        DU,
        "        if pe.failed {\n            pe.stats.deferred_dropped += deferred.len() as u64;\n            return;\n        }\n        pe.stats.deferred_sent",
        "        if false && pe.failed {\n            pe.stats.deferred_dropped += deferred.len() as u64;\n            return;\n        }\n        pe.stats.deferred_sent",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P13", "el destino acusa sin haber anotado la cuenta en su disco",
        CL,
        "            b.delete(k_tomb(player));\n            pe.apply_critical(&b);\n        }",
        "            b.delete(k_tomb(player));\n            let _ = &b;\n        }",
        PRUEBAS_ONLINE,
    ),
    Mutante(
        "P14", "el destino adopta a una cuenta que ya está conectada aquí con otra época (dos copias)",
        CL,
        "        if arrival.is_none() && self.by_entity.contains_key(&player) {",
        "        if false && arrival.is_none() && self.by_entity.contains_key(&player) {",
        PRUEBAS_ONLINE,
    ),
    # ------------------------------------------------------------------ el cifrado (0.6.0)
    Mutante(
        "C1", "Poly1305 no hace la reducción final módulo 2^130-5 (falla solo con ciertos valores)",
        AE,
        "        let mask = (g4 >> 31).wrapping_sub(1);",
        "        let mask = 0u32;",
        PRUEBAS_AEAD,
    ),
    Mutante(
        "C2", "los datos asociados no se rellenan hasta 16 bytes antes de autenticar (etiqueta distinta a la del estándar)",
        AE,
        "    pad16(&mut p, aad.len());\n",
        "",
        PRUEBAS_AEAD,
    ),
    Mutante(
        "C3", "open_in_place descifra y deja el texto en claro en el buffer aunque la etiqueta falle",
        AE,
        "    let expected = aead_tag(key, nonce, aad, buf);\n    if !ct_eq(&expected, tag) {\n        return false;\n    }\n    chacha20_xor(key, 1, nonce, buf);\n    true",
        "    let expected = aead_tag(key, nonce, aad, buf);\n    chacha20_xor(key, 1, nonce, buf);\n    if !ct_eq(&expected, tag) {\n        return false;\n    }\n    true",
        PRUEBAS_AEAD,
    ),
    Mutante(
        "C4", "el contador de bloque de ChaCha20 da la vuelta en silencio (el flujo de claves se repetiría)",
        AE,
        "        ctr = ctr\n            .checked_add(1)\n            .expect(\"ChaCha20: contador de bloque agotado\");",
        "        ctr = ctr.wrapping_add(1);",
        PRUEBAS_AEAD,
    ),
    Mutante(
        "C5", "el transporte descifra pero no comprueba la etiqueta de los DATA (acepta paquetes alterados)",
        WI,
        "    open_in_place(key, &nonce_for(counter), aad, &mut buf, t).then_some(buf)",
        "    let _ = open_in_place(key, &nonce_for(counter), aad, &mut buf, t);\n    Some(buf)",
        PRUEBAS_TRANSPORT,
    ),
    Mutante(
        "C6", "el transporte no comprueba la etiqueta de los DISCONNECT (cualquiera expulsa a un jugador)",
        WI,
        "    open_in_place(key, &nonce_for(counter), aad, &mut [], t)\n}",
        "    let _ = open_in_place(key, &nonce_for(counter), aad, &mut [], t);\n    true\n}",
        PRUEBAS_TRANSPORT,
    ),
    Mutante(
        "C7", "el nonce del transporte ignora el contador (todos los paquetes cifran con el mismo flujo)",
        CR,
        "    n[4..].copy_from_slice(&counter.to_le_bytes());",
        "    let _ = counter;",
        PRUEBAS_TRANSPORT,
    ),
    Mutante(
        "C8", "los dos sentidos de una sesión usan la misma clave (un paquete se puede reflejar hacia atrás)",
        CR,
        "        s2c: master.mac(&[b\"aead-s2c\"]),",
        "        s2c: master.mac(&[b\"aead-c2s\"]),",
        PRUEBAS_TRANSPORT,
    ),
    Mutante(
        "C9", "el servidor acepta un HELLO de cualquier versión del protocolo (también la anterior, sin cifrar)",
        TS,
        "        if pkt.len() < wire::HELLO_LEN || pkt[1] != wire::PROTOCOL_VERSION {",
        "        if pkt.len() < wire::HELLO_LEN {",
        PRUEBAS_TRANSPORT,
    ),
    Mutante(
        "C10", "todos los shards cifran el bus con la misma clave (la de «desde» no entra en la derivación)",
        UB,
        "            .mac(&[b\"tsofgw/bus-aead/v2\", &from.to_le_bytes()])",
        "            .mac(&[b\"tsofgw/bus-aead/v2\", &0u16.to_le_bytes()])",
        PRUEBAS_BUS,
    ),
    Mutante(
        "C11", "el nonce del bus ignora la sal de arranque (un reinicio con el reloj atrasado repetiría nonces)",
        UB,
        "    n[..4].copy_from_slice(&boot.to_le_bytes());",
        "    let _ = boot;",
        PRUEBAS_BUS,
    ),
    Mutante(
        "C12", "el bus descifra pero no comprueba la etiqueta (acepta datagramas forjados)",
        UB,
        "    open_in_place(\n        sender_key,\n        &nonce_of(p.head.boot, p.head.counter),\n        aad,\n        &mut buf,\n        tag,\n    )\n    .then_some(buf)",
        "    let _ = open_in_place(\n        sender_key,\n        &nonce_of(p.head.boot, p.head.counter),\n        aad,\n        &mut buf,\n        tag,\n    );\n    Some(buf)",
        PRUEBAS_BUS,
    ),
]


def raiz() -> Path:
    r = Path(__file__).resolve().parent.parent
    if not (r / "Cargo.toml").exists():
        sys.exit(f"no encuentro el repositorio en {r}")
    return r


def comprobar(root: Path, ms) -> int:
    """Cada parche debe aplicarse exactamente una vez; si no, el catálogo está desactualizado."""
    mal = 0
    ids = set()
    for m in ms:
        if m.id in ids:
            print(f"{m.id}: identificador repetido")
            mal += 1
        ids.add(m.id)
        src = (root / m.archivo).read_text(encoding="utf-8")
        n = src.count(m.viejo)
        if n != 1:
            print(f"{m.id}: el texto a sustituir aparece {n} veces en {m.archivo} (debe ser 1): {m.defecto}")
            mal += 1
        elif m.viejo == m.nuevo:
            print(f"{m.id}: el parche no cambia nada")
            mal += 1
    return mal


def cargo(root: Path, target: Path, args, extra, timeout):
    env = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_TERM_COLOR="never")
    cmd = ["cargo", "test", *extra, *args]
    t0 = time.time()
    try:
        r = subprocess.run(cmd, cwd=root, env=env, capture_output=True, text=True, timeout=timeout)
        out = r.stdout + r.stderr
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        out += "\n(tiempo agotado)"
        return False, True, [], out, time.time() - t0
    compila = "could not compile" not in out and "error[" not in out
    falladas = re.findall(r"^test (\S+) \.\.\. FAILED", out, re.M)
    return r.returncode == 0, compila, falladas, out, time.time() - t0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true", help="solo comprueba que los parches aún se aplican")
    ap.add_argument("--list", action="store_true", help="lista los mutantes y termina")
    ap.add_argument("--only", help="identificadores separados por comas (por ejemplo P1,P13,S5)")
    ap.add_argument("--cargo-args", default="", help="argumentos extra para `cargo test` (por ejemplo --offline)")
    ap.add_argument("--target-dir", help="carpeta de compilación (por defecto, una temporal junto a la copia)")
    ap.add_argument("--timeout", type=int, default=1500, help="segundos máximos por mutante")
    ap.add_argument("--keep", action="store_true", help="no borra la copia temporal al terminar")
    a = ap.parse_args()

    root = raiz()
    ms = MUTANTES
    if a.only:
        pedidos = [x.strip() for x in a.only.split(",") if x.strip()]
        conocidos = {m.id for m in MUTANTES}
        for x in pedidos:
            if x not in conocidos:
                sys.exit(f"mutante desconocido: {x}")
        ms = [m for m in MUTANTES if m.id in pedidos]

    if a.list:
        for m in ms:
            print(f"{m.id:>4}  {m.defecto}")
        return 0

    mal = comprobar(root, ms)
    if mal:
        print(f"\n{mal} problema(s) en el catálogo: actualiza ci/mutaciones.py")
        return 1
    if a.check:
        print(f"ok: los {len(ms)} parches se aplican exactamente una vez")
        return 0

    extra = a.cargo_args.split()
    tmp = Path(tempfile.mkdtemp(prefix="tsofgw-mutaciones-"))
    copia = tmp / "repo"
    target = Path(a.target_dir) if a.target_dir else tmp / "target"
    try:
        print(f"copiando el repositorio a {copia} ...")
        # `shutil.copy` (y no `copy2`): los archivos copiados llevan la hora de ahora. Con la hora de
        # origen, cargo daría por «al día» artefactos compilados antes por otra copia o por otro mutante,
        # y la prueba se ejecutaría contra código que ya no es el de los fuentes.
        shutil.copytree(
            root,
            copia,
            ignore=shutil.ignore_patterns("target", ".git", "__pycache__"),
            copy_function=shutil.copy,
        )

        # 1. Sin defectos, todo debe pasar (una vez por cada grupo de pruebas que se vaya a usar).
        vistos = []
        for m in ms:
            if m.pruebas in vistos:
                continue
            vistos.append(m.pruebas)
            nombre = " ".join(m.pruebas)
            print(f"base ({nombre}): ", end="", flush=True)
            ok, compila, falladas, out, dt = cargo(copia, target, m.pruebas, extra, a.timeout)
            if not ok:
                print(f"FALLA sin defectos ({dt:.0f} s): no se puede probar nada así.")
                print(out[-3000:])
                return 2
            print(f"pasa ({dt:.0f} s)")

        # 2. Un defecto cada vez.
        resultados = []
        for m in ms:
            ruta = copia / m.archivo
            original = ruta.read_text(encoding="utf-8")
            ruta.write_text(original.replace(m.viejo, m.nuevo, 1), encoding="utf-8")
            try:
                ok, compila, falladas, out, dt = cargo(copia, target, m.pruebas, extra, a.timeout)
            finally:
                ruta.write_text(original, encoding="utf-8")
            if not compila:
                estado = "NO COMPILA"
            elif falladas or not ok:
                estado = "DETECTADO"
            else:
                estado = "SUPERVIVIENTE"
            resultados.append((m, estado, falladas))
            quien = ", ".join(falladas[:3]) + (" ..." if len(falladas) > 3 else "") if falladas else ""
            print(f"{m.id:>4} {estado:<13} {dt:5.0f} s  {m.defecto}" + (f"\n        la detecta: {quien}" if quien else ""), flush=True)
            if estado == "NO COMPILA":
                print(out[-1500:])

        malos = [m.id for m, e, _ in resultados if e != "DETECTADO"]
        print()
        print(f"{len(resultados) - len(malos)} de {len(resultados)} defectos detectados.")
        if malos:
            print("Sin detectar o sin compilar: " + ", ".join(malos))
            return 1
        return 0
    finally:
        if a.keep:
            print(f"copia conservada en {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
