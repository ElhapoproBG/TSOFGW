#include <stdint.h>
#include <stddef.h>
/* FNV-1a 64 bits: identificador rápido NO criptográfico. Para seguridad se usa SHA-256 (tsofgw-pack). */
uint64_t tsofgw_hash64(const uint8_t *data, size_t len) {
    uint64_t h = 0xcbf29ce484222325ULL; /* base FNV-1a de 64 bits */
    for (size_t i = 0; i < len; i++) { h ^= data[i]; h *= 1099511628211ULL; }
    return h;
}
