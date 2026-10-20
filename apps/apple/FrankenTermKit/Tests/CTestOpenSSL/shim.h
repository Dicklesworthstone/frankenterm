#include <openssl/evp.h>
#include <openssl/sha.h>
#include <openssl/hmac.h>
#include <openssl/ec.h>
#include <openssl/bn.h>
#include <openssl/obj_mac.h>

/* Independent test oracle: only maintained OpenSSL group/scalar operations.
 * No EC_KEY deprecated API and no production dependency on OpenSSL. */
static inline int ft_test_ecdsa_public_key(int curve_nid,
    const unsigned char *scalar_bytes, size_t scalar_len,
    unsigned char *public_bytes, size_t public_len) {
    if (!scalar_bytes || !public_bytes || scalar_len == 0 || scalar_len > 66 ||
        public_len != 1 + 2 * scalar_len) return 0;
    int result = 0;
    EC_GROUP *group = EC_GROUP_new_by_curve_name(curve_nid);
    EC_POINT *point = group ? EC_POINT_new(group) : NULL;
    BIGNUM *scalar = BN_bin2bn(scalar_bytes, (int)scalar_len, NULL);
    BIGNUM *order = BN_new();
    if (group && point && scalar && order &&
        EC_GROUP_get_order(group, order, NULL) == 1 &&
        !BN_is_zero(scalar) && !BN_is_negative(scalar) && BN_cmp(scalar, order) < 0 &&
        EC_POINT_mul(group, point, scalar, NULL, NULL, NULL) == 1 &&
        EC_POINT_point2oct(group, point, POINT_CONVERSION_UNCOMPRESSED,
                          public_bytes, public_len, NULL) == public_len) {
        result = 1;
    }
    BN_clear_free(scalar);
    BN_free(order);
    EC_POINT_free(point);
    EC_GROUP_free(group);
    return result;
}

/* Independent AES counter-mode primitive for Linux fixture tests. */
static inline int ft_test_aes_ctr(const unsigned char *ciphertext, size_t ciphertext_len,
    const unsigned char *key, size_t key_len, const unsigned char *iv, size_t iv_len,
    unsigned char *output, size_t output_len) {
    if (!ciphertext || !key || !iv || !output || !ciphertext_len ||
        ciphertext_len > 65536 || ciphertext_len % 16 != 0 ||
        (key_len != 16 && key_len != 32) || iv_len != 16 || output_len != ciphertext_len) return 0;
    EVP_CIPHER_CTX *context = EVP_CIPHER_CTX_new();
    int written = 0, final_written = 0;
    int ok = context &&
        EVP_DecryptInit_ex(context, key_len == 16 ? EVP_aes_128_ctr() : EVP_aes_256_ctr(), NULL, key, iv) == 1 &&
        EVP_CIPHER_CTX_set_padding(context, 0) == 1 &&
        EVP_DecryptUpdate(context, output, &written, ciphertext, (int)ciphertext_len) == 1 &&
        written >= 0 && (size_t)written <= output_len &&
        EVP_DecryptFinal_ex(context, output + written, &final_written) == 1 &&
        (size_t)written == ciphertext_len && final_written == 0;
    EVP_CIPHER_CTX_free(context);
    if (!ok) OPENSSL_cleanse(output, output_len);
    return ok;
}
