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
