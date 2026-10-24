#include <openssl/evp.h>
#include <openssl/sha.h>
#include <openssl/hmac.h>
#include <openssl/ec.h>
#include <openssl/bn.h>
#include <openssl/obj_mac.h>
#include <openssl/core_names.h>
#include <openssl/params.h>

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

/* Independent SSH signature verification. A negative result is an oracle/setup
 * failure, zero is a cryptographic rejection, and one is a valid signature.
 * These helpers only consume public data and never enter an Apple product. */
static inline int ft_test_verify_ed25519(const unsigned char *public_bytes, size_t public_len,
    const unsigned char *message, size_t message_len,
    const unsigned char *signature, size_t signature_len) {
    if (!public_bytes || public_len != 32 || !message || message_len == 0 ||
        message_len > 65536 || !signature || signature_len != 64) return -1;
    EVP_PKEY *key = EVP_PKEY_new_raw_public_key(EVP_PKEY_ED25519, NULL, public_bytes, public_len);
    EVP_MD_CTX *context = EVP_MD_CTX_new();
    int result = -1;
    if (key && context && EVP_DigestVerifyInit(context, NULL, NULL, NULL, key) == 1) {
        result = EVP_DigestVerify(context, signature, signature_len, message, message_len);
    }
    EVP_MD_CTX_free(context);
    EVP_PKEY_free(key);
    return result;
}

static inline int ft_test_verify_ecdsa(int curve_nid, int digest_bits,
    const unsigned char *public_bytes, size_t public_len,
    const unsigned char *message, size_t message_len,
    const unsigned char *r_bytes, size_t r_len,
    const unsigned char *s_bytes, size_t s_len) {
    char p256[] = "prime256v1", p384[] = "secp384r1", p521[] = "secp521r1";
    char *group_name;
    size_t width;
    if (curve_nid == NID_X9_62_prime256v1) { group_name = p256; width = 32; }
    else if (curve_nid == NID_secp384r1) { group_name = p384; width = 48; }
    else if (curve_nid == NID_secp521r1) { group_name = p521; width = 66; }
    else return -1;
    const EVP_MD *digest = digest_bits == 256 ? EVP_sha256() :
        digest_bits == 384 ? EVP_sha384() : digest_bits == 512 ? EVP_sha512() : NULL;
    if (!digest || !public_bytes || public_len != 1 + 2 * width ||
        !message || message_len == 0 || message_len > 65536 ||
        !r_bytes || r_len == 0 || r_len > width || !s_bytes || s_len == 0 || s_len > width) return -1;

    EVP_PKEY_CTX *key_context = EVP_PKEY_CTX_new_from_name(NULL, "EC", NULL);
    EVP_PKEY *key = NULL;
    EVP_MD_CTX *context = EVP_MD_CTX_new();
    ECDSA_SIG *signature = ECDSA_SIG_new();
    BIGNUM *r = BN_bin2bn(r_bytes, (int)r_len, NULL);
    BIGNUM *s = BN_bin2bn(s_bytes, (int)s_len, NULL);
    OSSL_PARAM parameters[] = {
        OSSL_PARAM_construct_utf8_string(OSSL_PKEY_PARAM_GROUP_NAME, group_name, 0),
        OSSL_PARAM_construct_octet_string(OSSL_PKEY_PARAM_PUB_KEY, (void *)public_bytes, public_len),
        OSSL_PARAM_construct_end()
    };
    int result = -1;
    if (key_context && context && signature && r && s &&
        EVP_PKEY_fromdata_init(key_context) == 1 &&
        EVP_PKEY_fromdata(key_context, &key, EVP_PKEY_PUBLIC_KEY, parameters) == 1 &&
        ECDSA_SIG_set0(signature, r, s) == 1) {
        r = NULL; s = NULL; /* ECDSA_SIG owns both integers after set0 succeeds. */
        unsigned char der[160], *cursor = der;
        int length = i2d_ECDSA_SIG(signature, NULL);
        if (length > 0 && (size_t)length <= sizeof(der) &&
            i2d_ECDSA_SIG(signature, &cursor) == length &&
            EVP_DigestVerifyInit(context, NULL, digest, NULL, key) == 1) {
            result = EVP_DigestVerify(context, der, (size_t)length, message, message_len);
        }
    }
    BN_free(r); BN_free(s);
    ECDSA_SIG_free(signature);
    EVP_MD_CTX_free(context);
    EVP_PKEY_free(key);
    EVP_PKEY_CTX_free(key_context);
    return result;
}
