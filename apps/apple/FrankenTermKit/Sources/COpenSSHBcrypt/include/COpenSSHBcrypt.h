#ifndef FRANKENTERM_OPENSSH_BCRYPT_H
#define FRANKENTERM_OPENSSH_BCRYPT_H

#include <stddef.h>
#include <stdint.h>

/* The callback is used synchronously and never retained. Supply a maintained
 * SHA-512 implementation that writes exactly 64 bytes. No global callback
 * registration or cryptographic state is shared between operations. */
typedef void (*ft_bcrypt_sha512_fn)(uint8_t *output,
    const uint8_t *input, size_t input_length);

/* Returns 0 on success, -1 on rejected input or allocation failure.
 * Inputs are bounded before work: passphrase 1...1024 bytes, salt 16...64,
 * rounds 1...64, and output exactly 32 or 48 bytes for an AES key plus IV.
 * This bounded synchronous call is not interruptible mid-round. */
int ft_openssh_bcrypt_pbkdf(const uint8_t *pass, size_t passlen,
    const uint8_t *salt, size_t saltlen, uint8_t *key, size_t keylen,
    uint32_t rounds, ft_bcrypt_sha512_fn sha512);

#ifdef __APPLE__
/* Apple CommonCrypto AES-CTR, big-endian counter, no padding. Returns 1 only
 * for complete, exact-length output; no private output survives failure.
 * This primitive is used solely inside source-key container validation. */
int ft_openssh_aes_ctr(const uint8_t *ciphertext, size_t ciphertext_length,
    const uint8_t *key, size_t key_length, const uint8_t *iv, size_t iv_length,
    uint8_t *output, size_t output_length);
#endif

#endif

