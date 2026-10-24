#ifndef FRANKENTERM_BCRYPT_SHA512_ADAPTER_H
#define FRANKENTERM_BCRYPT_SHA512_ADAPTER_H

#define crypto_hash_sha512_BYTES 64
/* sha512 is the per-operation callback parameter of the vendored PBKDF. */
#define crypto_hash_sha512(output, input, count) sha512((output), (const uint8_t *)(input), (count))

#endif

