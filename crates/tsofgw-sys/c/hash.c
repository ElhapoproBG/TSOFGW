#include <stdint.h>
#include <stddef.h>
/* FNV-1a 64 bits: hash de contenido de ejemplo para identificar módulos. */
uint64_t tsofgw_hash64(const uint8_t *data, size_t len) {
    uint64_t h = 1469598103934665603ULL;
    for (size_t i = 0; i < len; i++) { h ^= data[i]; h *= 1099511628211ULL; }
    return h;
}
