#!/usr/bin/env python3
"""Genera `crates/tsofgw-aead/tests/vectores/mod.rs` con una implementación independiente.

Los vectores salen de OpenSSL 3 (a través de la biblioteca `cryptography` de Python), con una semilla fija:
volver a ejecutar este script debe dar el mismo archivo. Sirven para comprobar que `tsofgw-aead` (que es
código propio, sin auditar) coincide con una implementación madura en todo lo que se pueda forzar:
longitudes en los bordes de bloque, datos asociados de varios tamaños, mensajes de todo 0xFF, claves
extremas y las llamadas «reducciones finales» de Poly1305 (cuando el acumulador acaba entre 2^130-5 y
2^130-1, un caso que casi ningún mensaje aleatorio alcanza: se construye a propósito con r pequeño y bloques
de 0xFF, como los vectores A.3 del RFC 8439).

Uso:  pip install cryptography  &&  python ci/generar_vectores_aead.py
"""
import random
from pathlib import Path

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey, X25519PublicKey
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.poly1305 import Poly1305

rnd = random.Random(20261125)


def rb(n):
    return bytes(rnd.getrandbits(8) for _ in range(n))


# --- AEAD: longitudes en los bordes de bloque, datos asociados de tamaño variable
aead = []
for i, L in enumerate([0, 1, 2, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 191, 192, 193, 255, 256, 257, 1000, 1183]):
    k, n = rb(32), rb(12)
    aad = rb([0, 1, 12, 16, 17, 33][i % 6])
    pt = rb(L)
    aead.append((k, n, aad, pt, ChaCha20Poly1305(k).encrypt(n, pt, aad)))

# --- Poly1305: claves y mensajes extremos
poly = []
keys = [
    bytes([0xFF] * 32),
    bytes([0x00] * 32),
    bytes([0xFF] * 16) + bytes([0] * 16),
    bytes([0] * 16) + bytes([0xFF] * 16),
    bytes.fromhex("ffffff0fffffff0fffffff0fffffff0f" + "ff" * 16),
]
for _ in range(6):
    keys.append(rb(32))
msgs = [b"", b"\xff" * 16, b"\xff" * 17, b"\xff" * 32, b"\xff" * 47, b"\xff" * 64, b"\x00" * 16, b"\x00" * 33,
        b"\xfb" + b"\xfe" * 15, b"\xff" * 1024]
for i, k in enumerate(keys):
    for j, m in enumerate(msgs):
        if (i + j) % 2 == 0 or i < 5:
            poly.append((k, m, Poly1305.generate_tag(k, m)))

# Reducción final: con r pequeño y bloques de 0xFF el acumulador acaba en [2^130-5, 2^130-1] (≥ p).
for r in list(range(1, 17)) + [0x0FFFFFFC, 0x0FFFFFFC0FFFFFFC]:
    for s in (bytes(16), bytes([0xFF] * 16), rb(16)):
        key = r.to_bytes(16, "little") + s
        for m in (b"\xff" * 16, b"\xff" * 32, b"\xff" * 48, b"\xff" * 17, b"\xff" * 33, b"\xff" * 15, b"\xfb" + b"\xff" * 15):
            poly.append((key, m, Poly1305.generate_tag(key, m)))

# --- Bloques de ChaCha20 con contadores extremos
blocks = []
for ctr in [0, 1, 2, 255, 0xFFFFFFFF, 0x80000000]:
    k, n = rb(32), rb(12)
    enc = Cipher(algorithms.ChaCha20(k, ctr.to_bytes(4, "little") + n), mode=None).encryptor()
    blocks.append((k, ctr, n, enc.update(bytes(64))))

# --- X25519: claves privadas aleatorias y extremas contra claves públicas aleatorias y de borde.
# (escalar, u, resultado). Se omiten los puntos de orden pequeño: OpenSSL se niega a calcularlos.
xdh = []
scalars = [bytes(32), bytes([0xFF] * 32), bytes([0xF8] + [0] * 30 + [0x40]), bytes([0x07] + [0xFF] * 30 + [0x7F])]
scalars += [rb(32) for _ in range(40)]
us = [bytes([9] + [0] * 31), bytes([0xFF] * 31 + [0x7F]), bytes([2] + [0] * 31), bytes([0xEC] + [0xFF] * 30 + [0x7F])]
us += [rb(32) for _ in range(30)]
for i, k in enumerate(scalars):
    for j, u in enumerate(us):
        if (i + j) % 3 and i >= 4:
            continue
        try:
            r = X25519PrivateKey.from_private_bytes(k).exchange(X25519PublicKey.from_public_bytes(u))
        except ValueError:
            continue  # resultado todo ceros: lo rechaza OpenSSL (y `shared_secret` también)
        xdh.append((k, u, r))


def arr(name, ty, items):
    out = f"pub const {name}: &[{ty}] = &[\n"
    for it in items:
        out += "    " + it + ",\n"
    return out + "];\n"


h = bytes.hex
out = "//! Vectores generados con OpenSSL 3 (biblioteca `cryptography` de Python), semilla fija.\n"
out += "//! NO editar a mano: `python ci/generar_vectores_aead.py`.\n\n"
out += "/// (clave, nonce, aad, texto en claro, texto cifrado ‖ etiqueta)\n"
out += arr("AEAD", "(&str, &str, &str, &str, &str)",
           [f'("{h(k)}", "{h(n)}", "{h(a)}", "{h(p)}", "{h(c)}")' for k, n, a, p, c in aead])
out += "\n/// (clave de un solo uso, mensaje, etiqueta)\n"
out += arr("POLY1305", "(&str, &str, &str)", [f'("{h(k)}", "{h(m)}", "{h(t)}")' for k, m, t in poly])
out += "\n/// (clave, contador de bloque, nonce, bloque de 64 bytes)\n"
out += arr("BLOCKS", "(&str, u32, &str, &str)", [f'("{h(k)}", {c}, "{h(n)}", "{h(b)}")' for k, c, n, b in blocks])
out += "\n/// (escalar, u, X25519(escalar, u)) sin los resultados todo ceros\n"
out += arr("X25519", "(&str, &str, &str)", [f'("{h(k)}", "{h(u)}", "{h(r)}")' for k, u, r in xdh])
dest = Path(__file__).resolve().parent.parent / "crates/tsofgw-aead/tests/vectores/mod.rs"
dest.write_text(out, encoding="utf-8")
print(f"{len(aead)} AEAD, {len(poly)} Poly1305, {len(blocks)} bloques, {len(xdh)} X25519 -> {dest}")
