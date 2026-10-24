#ifndef FRANKENTERM_BCRYPT_PORTABILITY_H
#define FRANKENTERM_BCRYPT_PORTABILITY_H

#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include "include/COpenSSHBcrypt.h"

/* Only the vendored implementation sees these names. */
#define Blowfish_encipher ft_bcrypt_Blowfish_encipher
#define Blowfish_decipher ft_bcrypt_Blowfish_decipher
#define Blowfish_initstate ft_bcrypt_Blowfish_initstate
#define Blowfish_expand0state ft_bcrypt_Blowfish_expand0state
#define Blowfish_expandstate ft_bcrypt_Blowfish_expandstate
#define Blowfish_stream2word ft_bcrypt_Blowfish_stream2word
#define blf_key ft_bcrypt_blf_key
#define blf_enc ft_bcrypt_blf_enc
#define blf_dec ft_bcrypt_blf_dec
#define blf_ecb_encrypt ft_bcrypt_blf_ecb_encrypt
#define blf_ecb_decrypt ft_bcrypt_blf_ecb_decrypt
#define blf_cbc_encrypt ft_bcrypt_blf_cbc_encrypt
#define blf_cbc_decrypt ft_bcrypt_blf_cbc_decrypt

/* Volatile writes preserve clearing of buffers owned by this C target. They
 * do not claim to erase caller copies or all Swift/platform allocations. */
static inline void ft_bcrypt_clear(void *bytes, size_t count) {
    volatile unsigned char *cursor = (volatile unsigned char *)bytes;
    while (count-- != 0) *cursor++ = 0;
}
static inline void ft_bcrypt_freezero(void *bytes, size_t count) {
    if (bytes != NULL) { ft_bcrypt_clear(bytes, count); free(bytes); }
}
#define explicit_bzero ft_bcrypt_clear
#define freezero ft_bcrypt_freezero

#endif

