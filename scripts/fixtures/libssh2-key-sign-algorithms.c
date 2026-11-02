/* Narrow diagnostic for the actual patched libssh2 helpers, extracted by
 * test-libssh2-auth-patch.py. Only allocation, session metadata and crypto
 * capability discovery are mocked; negotiation and signature-name conversion
 * are compiled directly from the pinned and patched upstream source. */
#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define LIBSSH2_ERROR_NONE 0
#define LIBSSH2_ERROR_ALLOC (-6)
#define LIBSSH2_ERROR_METHOD_NONE (-33)
#define LIBSSH2_ERROR_INVAL (-34)
#define LIBSSH2_API
#define LIBSSH2_FRANKENTERM_PUBLICKEY_POLICY_VERSION 1
#define LIBSSH2_FRANKENTERM_PUBLICKEY_ALGORITHMS_MAX 1024
#define LIBSSH2_STATE_AUTHENTICATED 1
#define libssh2_NB_state_idle 0
#define GUARD_BYTES 128

typedef struct {
    const char *server_sign_algorithms;
    const char *sign_algo_prefs;
    const char *supported_algorithms;
    const char *banner;
    size_t allocations, fail_allocation, live_allocations;
    int last_error;
    int userauth_pblc_state, state, ft_publickey_policy_active;
    size_t ft_publickey_policy_length;
    char ft_publickey_algorithms[LIBSSH2_FRANKENTERM_PUBLICKEY_ALGORITHMS_MAX];
} LIBSSH2_SESSION;

struct allocation {
    size_t length;
    unsigned char data[];
};

static void *checked_alloc(LIBSSH2_SESSION *session, size_t length)
{
    struct allocation *allocation;
    session->allocations++;
    if(session->fail_allocation == session->allocations)
        return NULL;
    allocation = malloc(sizeof(*allocation) + length + GUARD_BYTES);
    assert(allocation);
    allocation->length = length;
    memset(allocation->data + length, 0xa5, GUARD_BYTES);
    session->live_allocations++;
    return allocation->data;
}

static void checked_free(LIBSSH2_SESSION *session, void *pointer)
{
    struct allocation *allocation;
    size_t i;
    if(!pointer)
        return;
    allocation = (struct allocation *)((unsigned char *)pointer -
                                       offsetof(struct allocation, data));
    /* A repeated peer algorithm must never overrun the supported-list bound. */
    for(i = 0; i < GUARD_BYTES; i++)
        assert(allocation->data[allocation->length + i] == 0xa5);
    assert(session->live_allocations > 0);
    session->live_allocations--;
    free(allocation);
}

#define LIBSSH2_ALLOC(session, length) checked_alloc(session, length)
#define LIBSSH2_FREE(session, pointer) checked_free(session, pointer)

static const char *
_libssh2_supported_key_sign_algorithms(LIBSSH2_SESSION *session,
                                      const unsigned char *method,
                                      size_t method_length)
{
    (void)method;
    (void)method_length;
    return session->supported_algorithms;
}

static const char *libssh2_session_banner_get(LIBSSH2_SESSION *session)
{
    return session->banner;
}

static int _libssh2_error(LIBSSH2_SESSION *session, int code,
                          const char *message)
{
    (void)message;
    session->last_error = code;
    return code;
}

#include "libssh2-algorithms-under-test.h"

static unsigned int case_count;

static void run_case(const char *name, const char *input, const char *server,
                     const char *preferences, const char *banner,
                     const char *supported, size_t fail_allocation,
                     int expected_error, const char *request,
                     const char *signature)
{
    LIBSSH2_SESSION session = {0};
    size_t length = strlen(input);
    unsigned char *method = checked_alloc(&session, length);
    int result;
    memcpy(method, input, length);
    session.server_sign_algorithms = server;
    session.sign_algo_prefs = preferences;
    session.banner = banner;
    session.supported_algorithms = supported;
    session.allocations = 0;
    session.fail_allocation = fail_allocation;
    result = _libssh2_key_sign_algorithm(&session, &method, &length);
    if(result != expected_error) {
        fprintf(stderr, "%s: result %d, expected %d\n", name, result,
                expected_error);
        exit(1);
    }
    if(request) {
        assert(method && length == strlen(request));
        assert(memcmp(method, request, length) == 0);
    }
    else {
        assert(!method && length == 0);
    }
    if(signature) {
        /* The request's certificate suffix must not reach the signature blob. */
        length = plain_method((char *)method, length);
        assert(length == strlen(signature));
        assert(memcmp(method, signature, length) == 0);
    }
    checked_free(&session, method);
    assert(session.live_allocations == 0);
    case_count++;
}

static void test_exact_publickey_policy_after_selection(void)
{
    LIBSSH2_SESSION session = {0};
    const char *cert = "ssh-rsa-cert-v01@openssh.com";
    const char *allowed = "rsa-sha2-256-cert-v01@openssh.com";
    size_t length = strlen(cert);
    unsigned char *method = checked_alloc(&session, length);
    memcpy(method, cert, length);
    session.supported_algorithms = "rsa-sha2-512,rsa-sha2-256,ssh-rsa";
    session.sign_algo_prefs = "rsa-sha2-256";
    session.banner = "SSH-2.0-OpenSSH_9.9";
    assert(libssh2_frankenterm_publickey_policy_version() == 1);
    assert(libssh2_frankenterm_publickey_policy(&session, allowed, strlen(allowed)) == 0);

    /* No server-sig-algs leaves the original SHA-1 request unchanged. The
     * exact post-selection guard must deny it before the offer, independent
     * of the sign callback that would run only after the peer accepted it. */
    assert(_libssh2_key_sign_algorithm(&session, &method, &length) == 0);
    assert(!frankenterm_publickey_algorithm_permitted(&session, method, length));
    session.server_sign_algorithms = "rsa-sha2-512,rsa-sha2-256";
    assert(_libssh2_key_sign_algorithm(&session, &method, &length) == 0);
    assert(length == strlen(allowed) && !memcmp(method, allowed, length));
    assert(frankenterm_publickey_algorithm_permitted(&session, method, length));
    assert(!frankenterm_publickey_algorithm_permitted(&session,
        (const unsigned char *)"rsa-sha2-256", 12));

    /* The fallback's default key name is subject to the same exact check. */
    assert(!frankenterm_publickey_algorithm_permitted(&session,
        (const unsigned char *)cert, strlen(cert)));
    assert(libssh2_frankenterm_publickey_policy(&session, "", 0) == 0);
    assert(!frankenterm_publickey_algorithm_permitted(&session, method, length));
    assert(libssh2_frankenterm_publickey_policy(&session, "unknown-key", 11) == 0);
    assert(!frankenterm_publickey_algorithm_permitted(&session, method, length));

    /* Invalid mutation preserves the previous restriction; an in-flight or
     * already authenticated session cannot have its policy replaced. */
    assert(libssh2_frankenterm_publickey_policy(&session, "rsa-sha2-256,", 13) == LIBSSH2_ERROR_INVAL);
    assert(!frankenterm_publickey_algorithm_permitted(&session, method, length));
    session.userauth_pblc_state = 1;
    assert(libssh2_frankenterm_publickey_policy(&session, NULL, 0) == LIBSSH2_ERROR_INVAL);
    session.userauth_pblc_state = 0; session.state = LIBSSH2_STATE_AUTHENTICATED;
    assert(libssh2_frankenterm_publickey_policy(&session, NULL, 0) == LIBSSH2_ERROR_INVAL);
    session.state = 0;
    assert(libssh2_frankenterm_publickey_policy(&session, NULL, 0) == 0);
    assert(frankenterm_publickey_algorithm_permitted(&session, method, length));
    checked_free(&session, method);
    assert(session.live_allocations == 0);
    case_count++;
}

int main(void)
{
    const char *cert = "ssh-rsa-cert-v01@openssh.com";
    const char *cert256 = "rsa-sha2-256-cert-v01@openssh.com";
    const char *cert512 = "rsa-sha2-512-cert-v01@openssh.com";
    const char *supported = "rsa-sha2-512,rsa-sha2-256,ssh-rsa";
    const char *modern = "SSH-2.0-OpenSSH_9.9";

    run_case("RSA certificate SHA512", cert, supported, NULL, modern,
             supported, 0, 0, cert512, "rsa-sha2-512");
    run_case("RSA certificate SHA256", cert, "rsa-sha2-256", NULL, modern,
             supported, 0, 0, cert256, "rsa-sha2-256");
    run_case("explicit SHA256 preference", cert, supported,
             "rsa-sha2-256,rsa-sha2-512", modern,
             supported, 0, 0, cert256, "rsa-sha2-256");
    run_case("raw RSA remains raw", "ssh-rsa", supported, NULL, modern,
             supported, 0, 0, "rsa-sha2-512", "rsa-sha2-512");
    run_case("peer order cannot override preference", cert,
             "ssh-rsa,rsa-sha2-256,rsa-sha2-512", NULL, modern,
             supported, 0, 0, cert512, "rsa-sha2-512");
    run_case("duplicate peer algorithms remain bounded", cert,
             "rsa-sha2-512,rsa-sha2-512,rsa-sha2-512,rsa-sha2-256", NULL,
             modern, supported, 0, 0, cert512, "rsa-sha2-512");
    run_case("no algorithm intersection", cert, "ssh-ed25519", NULL, modern,
             supported, 0, LIBSSH2_ERROR_METHOD_NONE, cert, NULL);
    run_case("explicit preference cannot widen peer set", cert,
             "rsa-sha2-256", "rsa-sha2-512", modern, supported, 0,
             LIBSSH2_ERROR_METHOD_NONE, cert, NULL);
    run_case("unsupported key unchanged", "ssh-ed25519", supported, NULL,
             modern, NULL, 0, 0, "ssh-ed25519", "ssh-ed25519");
    run_case("no extension unchanged", cert, NULL, NULL, modern,
             supported, 0, 0, cert, "ssh-rsa");
    run_case("legacy cert path frees its filter", cert, supported, NULL,
             "SSH-2.0-OpenSSH_7.7", supported, 0, 0, cert, "ssh-rsa");
    run_case("legacy raw RSA still upgrades", "ssh-rsa", supported, NULL,
             "SSH-2.0-OpenSSH_7.7", supported, 0, 0,
             "rsa-sha2-512", "rsa-sha2-512");
    run_case("7.8 certificate upgrades", cert, supported, NULL,
             "SSH-2.0-OpenSSH_7.8", supported, 0, 0, cert512, "rsa-sha2-512");
    run_case("same-length other name is not an RSA cert",
             "xsh-rsa-cert-v01@openssh.com", supported, NULL, modern,
             supported, 0, 0, "rsa-sha2-512", "rsa-sha2-512");
    run_case("truncated name is not an RSA cert",
             "ssh-rsa-cert-v01@openssh.co", supported, NULL, modern,
             supported, 0, 0, "rsa-sha2-512", "rsa-sha2-512");
    run_case("filter allocation failure preserves input", cert,
             supported, NULL, modern, supported, 1,
             LIBSSH2_ERROR_ALLOC, cert, NULL);
    run_case("certificate upgrade allocation failure clears method", cert,
             supported, NULL, modern, supported, 2,
             LIBSSH2_ERROR_ALLOC, NULL, NULL);
    run_case("raw upgrade allocation failure clears method", "ssh-rsa",
             supported, NULL, modern, supported, 2,
             LIBSSH2_ERROR_ALLOC, NULL, NULL);
    test_exact_publickey_policy_after_selection();
    printf("Patched libssh2 negotiation and policy: %u cases passed\n", case_count);
    return 0;
}
