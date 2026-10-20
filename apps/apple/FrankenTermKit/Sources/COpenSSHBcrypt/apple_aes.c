#include "includes.h"

#ifdef __APPLE__
#include <CommonCrypto/CommonCryptor.h>

int
ft_openssh_aes_ctr(const uint8_t *ciphertext, size_t ciphertext_length,
    const uint8_t *key, size_t key_length, const uint8_t *iv, size_t iv_length,
    uint8_t *output, size_t output_length)
{
    if (ciphertext == NULL || key == NULL || iv == NULL || output == NULL ||
        ciphertext_length == 0 || ciphertext_length > 65536 ||
        ciphertext_length % 16 != 0 || (key_length != 16 && key_length != 32) ||
        iv_length != 16 || output_length != ciphertext_length)
        return 0;
    CCCryptorRef cryptor = NULL;
    CCCryptorStatus status = CCCryptorCreateWithMode(kCCDecrypt, kCCModeCTR,
        kCCAlgorithmAES, ccNoPadding, iv, key, key_length, NULL, 0, 0,
        kCCModeOptionCTR_BE, &cryptor);
    size_t written = 0, final_written = 0;
    if (status == kCCSuccess) {
        status = CCCryptorUpdate(cryptor, ciphertext, ciphertext_length,
            output, output_length, &written);
        if (status == kCCSuccess && written <= output_length) {
            status = CCCryptorFinal(cryptor, output + written,
                output_length - written, &final_written);
        }
    }
    if (cryptor != NULL) CCCryptorRelease(cryptor);
    if (status != kCCSuccess || written != ciphertext_length || final_written != 0) {
        ft_bcrypt_clear(output, output_length);
        return 0;
    }
    return 1;
}
#endif

