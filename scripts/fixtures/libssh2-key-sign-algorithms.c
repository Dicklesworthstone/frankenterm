/* Narrow diagnostic for the actual patched libssh2 helpers, extracted by
 * test-libssh2-auth-patch.py. Only allocation, session metadata and the
 * banner lookup are mocked. The RSA signature upgrade, the signature-name
 * conversion, the OpenSSH version parser and the public-key policy are
 * compiled directly from the pinned and patched upstream source, with the
 * prepared libssh2.h supplying its real constants and prototypes. */
#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "libssh2.h"

/* The Apple build's OpenSSL backend: RSA SHA-2 signatures on, SHA-1 off
 * (libssh2 1.11.2 defaults; LIBSSH2_RSA_SHA1_ENABLE is never set). */
#define LIBSSH2_RSA_SHA2 1
#define LIBSSH2_RSA_SHA1 0
#define SSH2_STATE_AUTHENTICATED 0x00000008
enum { ssh2_NB_state_idle = 0 };
#define GUARD_BYTES 128

struct _LIBSSH2_SESSION {
    char *server_sign_algorithms;
    char *sign_algo_prefs;
    const char *banner;
    size_t allocations, fail_allocation, live_allocations;
    int last_error;
    int userauth_pblc_state, state, ft_publickey_policy_active;
    size_t ft_publickey_policy_length;
    char ft_publickey_algorithms[LIBSSH2_FRANKENTERM_PUBLICKEY_ALGORITHMS_MAX];
};

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
    /* A repeated peer algorithm must never overrun the filtered-list bound. */
    for(i = 0; i < GUARD_BYTES; i++)
        assert(allocation->data[allocation->length + i] == 0xa5);
    assert(session->live_allocations > 0);
    session->live_allocations--;
    free(allocation);
}

#define SSH2_ALLOC(session, length) checked_alloc(session, length)
#define SSH2_FREE(session, pointer) checked_free(session, pointer)
#define SSH2_SAFEFREE(session, pointer) \
    do { checked_free(session, pointer); (pointer) = NULL; } while(0)

const char *libssh2_session_banner_get(LIBSSH2_SESSION *session)
{
    return session->banner;
}

static int ssh2_err(LIBSSH2_SESSION *session, int code, const char *message)
{
    (void)message;
    session->last_error = code;
    return code;
}

#include "libssh2-algorithms-under-test.h"

static unsigned int case_count;

static char *copy_method(LIBSSH2_SESSION *session, const char *input)
{
    size_t length = strlen(input);
    char *method = checked_alloc(session, length + 1);
    assert(method);
    memcpy(method, input, length + 1);
    return method;
}

static void run_case(const char *name, const char *input, const char *server,
                     const char *preferences, const char *banner,
                     size_t fail_allocation, int expected_error,
                     const char *request, const char *signature)
{
    LIBSSH2_SESSION session;
    char *method;
    int result;
    memset(&session, 0, sizeof(session));
    method = copy_method(&session, input);
    session.server_sign_algorithms = (char *)server;
    session.sign_algo_prefs = (char *)preferences;
    session.banner = banner;
    session.allocations = 0;
    session.fail_allocation = fail_allocation;
    result = userauth_key_sign_algs(&session, &method);
    if(result != expected_error) {
        fprintf(stderr, "%s: result %d, expected %d\n", name, result,
                expected_error);
        exit(1);
    }
    if(request) {
        if(!method || strcmp(method, request)) {
            fprintf(stderr, "%s: request %s, expected %s\n", name,
                    method ? method : "(none)", request);
            exit(1);
        }
    }
    else {
        assert(!method);
    }
    if(signature) {
        /* The request's certificate suffix must not reach the signature blob. */
        ssh2_userauth_plain_method(method);
        if(strcmp(method, signature)) {
            fprintf(stderr, "%s: signature %s, expected %s\n", name, method,
                    signature);
            exit(1);
        }
    }
    checked_free(&session, method);
    assert(session.live_allocations == 0);
    case_count++;
}

static int permitted(LIBSSH2_SESSION *session, const char *method)
{
    return frankenterm_publickey_algorithm_permitted(session, method,
                                                     strlen(method));
}

static void test_exact_publickey_policy_after_selection(void)
{
    LIBSSH2_SESSION session;
    const char *cert = "ssh-rsa-cert-v01@openssh.com";
    const char *allowed = "rsa-sha2-256-cert-v01@openssh.com";
    char *method;
    memset(&session, 0, sizeof(session));
    method = copy_method(&session, cert);
    session.sign_algo_prefs = (char *)"rsa-sha2-256";
    session.banner = "SSH-2.0-OpenSSH_9.9";
    assert(libssh2_frankenterm_publickey_policy_version() == 1);
    assert(permitted(&session, "ssh-rsa"));
    assert(libssh2_frankenterm_publickey_policy(&session, allowed,
                                                strlen(allowed)) == 0);

    /* No server-sig-algs leaves the original SHA-1 request unchanged. The
     * exact post-selection guard must deny it before the offer, independent
     * of the sign callback that would run only after the peer accepted it. */
    assert(userauth_key_sign_algs(&session, &method) == 0);
    assert(!strcmp(method, cert));
    assert(!permitted(&session, method));
    session.server_sign_algorithms = (char *)"rsa-sha2-512,rsa-sha2-256";
    assert(userauth_key_sign_algs(&session, &method) == 0);
    assert(!strcmp(method, allowed));
    assert(permitted(&session, method));
    assert(!permitted(&session, "rsa-sha2-256"));
    assert(!permitted(&session, ""));
    assert(!frankenterm_publickey_algorithm_permitted(&session, NULL, 0));

    /* The fallback's default key names are subject to the same exact check. */
    assert(!permitted(&session, cert));
    assert(!permitted(&session, "ssh-rsa"));
    assert(libssh2_frankenterm_publickey_policy(&session, "", 0) == 0);
    assert(!permitted(&session, method));
    assert(libssh2_frankenterm_publickey_policy(&session, "unknown-key",
                                                11) == 0);
    assert(!permitted(&session, method));

    /* Invalid mutation preserves the previous restriction; an in-flight or
     * already authenticated session cannot have its policy replaced. */
    assert(libssh2_frankenterm_publickey_policy(&session, "rsa-sha2-256,",
                                                13) == LIBSSH2_ERROR_INVAL);
    assert(libssh2_frankenterm_publickey_policy(&session, ",rsa-sha2-256",
                                                13) == LIBSSH2_ERROR_INVAL);
    assert(libssh2_frankenterm_publickey_policy(&session, "a b", 3) ==
           LIBSSH2_ERROR_INVAL);
    assert(libssh2_frankenterm_publickey_policy(&session, NULL, 1) ==
           LIBSSH2_ERROR_INVAL);
    assert(!permitted(&session, method));
    session.userauth_pblc_state = 1;
    assert(libssh2_frankenterm_publickey_policy(&session, NULL, 0) ==
           LIBSSH2_ERROR_INVAL);
    session.userauth_pblc_state = 0;
    session.state = SSH2_STATE_AUTHENTICATED;
    assert(libssh2_frankenterm_publickey_policy(&session, NULL, 0) ==
           LIBSSH2_ERROR_INVAL);
    session.state = 0;
    assert(libssh2_frankenterm_publickey_policy(&session, NULL, 0) == 0);
    assert(permitted(&session, method));
    checked_free(&session, method);
    assert(session.live_allocations == 0);
    case_count++;
}

int main(void)
{
    const char *cert = "ssh-rsa-cert-v01@openssh.com";
    const char *cert256 = "rsa-sha2-256-cert-v01@openssh.com";
    const char *cert512 = "rsa-sha2-512-cert-v01@openssh.com";
    const char *peer = "rsa-sha2-512,rsa-sha2-256,ssh-rsa";
    const char *modern = "SSH-2.0-OpenSSH_9.9";
    const int none = LIBSSH2_ERROR_METHOD_NONE;
    const int alloc = LIBSSH2_ERROR_ALLOC;

    run_case("RSA certificate SHA512", cert, peer, NULL, modern, 0, 0,
             cert512, "rsa-sha2-512");
    run_case("RSA certificate SHA256", cert, "rsa-sha2-256", NULL, modern, 0,
             0, cert256, "rsa-sha2-256");
    run_case("explicit SHA256 preference", cert, peer,
             "rsa-sha2-256,rsa-sha2-512", modern, 0, 0, cert256,
             "rsa-sha2-256");
    run_case("raw RSA upgrades", "ssh-rsa", peer, NULL, modern, 0, 0,
             "rsa-sha2-512", "rsa-sha2-512");
    run_case("peer order cannot override preference", cert,
             "ssh-rsa,rsa-sha2-256,rsa-sha2-512", NULL, modern, 0, 0,
             cert512, "rsa-sha2-512");
    run_case("duplicate peer algorithms remain bounded", cert,
             "rsa-sha2-512,rsa-sha2-512,rsa-sha2-512,rsa-sha2-256", NULL,
             modern, 0, 0, cert512, "rsa-sha2-512");
    run_case("no algorithm intersection", cert, "ssh-ed25519", NULL, modern,
             0, none, cert, NULL);
    run_case("explicit preference cannot widen peer set", cert,
             "rsa-sha2-256", "rsa-sha2-512", modern, 0, none, cert, NULL);
    run_case("SHA-1 only peer gets no RSA certificate signature", cert,
             "ssh-rsa", NULL, modern, 0, none, cert, NULL);
    run_case("SHA-1 only peer gets no raw RSA signature", "ssh-rsa",
             "ssh-rsa", NULL, modern, 0, none, "ssh-rsa", NULL);
    run_case("unsupported key unchanged", "ssh-ed25519", peer, NULL, modern,
             0, 0, "ssh-ed25519", "ssh-ed25519");
    run_case("ECDSA certificate unchanged",
             "ecdsa-sha2-nistp256-cert-v01@openssh.com", peer, NULL, modern,
             0, 0, "ecdsa-sha2-nistp256-cert-v01@openssh.com",
             "ecdsa-sha2-nistp256");
    run_case("no extension unchanged", cert, NULL, NULL, modern, 0, 0, cert,
             "ssh-rsa");
    run_case("legacy certificate path allocates nothing", cert, peer, NULL,
             "SSH-2.0-OpenSSH_7.7", 1, 0, cert, "ssh-rsa");
    run_case("legacy raw RSA still upgrades", "ssh-rsa", peer, NULL,
             "SSH-2.0-OpenSSH_7.7", 0, 0, "rsa-sha2-512", "rsa-sha2-512");
    run_case("7.8 certificate upgrades", cert, peer, NULL,
             "SSH-2.0-OpenSSH_7.8", 0, 0, cert512, "rsa-sha2-512");
    run_case("same-length other name is not an RSA key",
             "xsh-rsa-cert-v01@openssh.com", peer, NULL, modern, 1, 0,
             "xsh-rsa-cert-v01@openssh.com", "xsh-rsa-cert-v01@openssh.com");
    run_case("truncated name is not an RSA key",
             "ssh-rsa-cert-v01@openssh.co", peer, NULL, modern, 1, 0,
             "ssh-rsa-cert-v01@openssh.co", "ssh-rsa-cert-v01@openssh.co");
    run_case("filter allocation failure preserves input", cert, peer, NULL,
             modern, 1, alloc, cert, NULL);
    run_case("certificate upgrade allocation failure clears method", cert,
             peer, NULL, modern, 2, alloc, NULL, NULL);
    run_case("raw upgrade allocation failure clears method", "ssh-rsa", peer,
             NULL, modern, 2, alloc, NULL, NULL);
    test_exact_publickey_policy_after_selection();
    printf("Patched libssh2 negotiation and policy: %u cases passed\n",
           case_count);
    return 0;
}
