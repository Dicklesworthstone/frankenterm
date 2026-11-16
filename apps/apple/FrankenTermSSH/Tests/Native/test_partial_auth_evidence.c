/* Exercise the actual patched libssh2 parser, without a production test API.
 * Compile with the prepared src/include directories and generated config,
 * then link the same static libssh2 archive plus its crypto dependencies.
 */
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "userauth.c"

static size_t packet(unsigned char *out, const char *methods, size_t length, unsigned char partial) {
    out[0] = SSH_MSG_USERAUTH_FAILURE;
    out[1] = (unsigned char)(length >> 24); out[2] = (unsigned char)(length >> 16);
    out[3] = (unsigned char)(length >> 8); out[4] = (unsigned char)length;
    if(length) memcpy(out + 5, methods, length);
    out[5 + length] = partial;
    return length + 6;
}
static void none(LIBSSH2_SESSION *s) {
    libssh2_uint64_t attempt = 7; int partial = 7; size_t length = 7;
    char methods[1025] = {7};
    assert(libssh2_frankenterm_publickey_unsigned_refusal(s) == 0);
    assert(libssh2_frankenterm_publickey_failure(s, &attempt, &partial, methods, sizeof(methods), &length) == 0);
    assert(!attempt && !partial && !length && !methods[0]);
}
static void snapshot(LIBSSH2_SESSION *s, libssh2_uint64_t expected, const char *wanted, int flag) {
    libssh2_uint64_t attempt = 0; int partial = 0; size_t length = 0;
    char methods[1025];
    s->err_code = -1234;
    assert(libssh2_frankenterm_publickey_unsigned_refusal(s) == 0);
    assert(libssh2_frankenterm_publickey_failure(s, &attempt, &partial, methods, sizeof(methods), &length) == 1);
    assert(attempt == expected && partial == flag && length == strlen(wanted) && !strcmp(methods, wanted));
    assert(s->err_code == -1234);
    none(s);
}
static void signed_and_unsigned(void) {
    LIBSSH2_SESSION s = {0}; s.ft_auth_attempt = 9;
    unsigned char wire[128]; const unsigned char booleans[] = {0, 1, 2, 255};
    const char *methods = "password,keyboard-interactive";
    for(size_t i = 0; i < sizeof(booleans); i++) {
        size_t n = packet(wire, methods, strlen(methods), booleans[i]);
        frankenterm_record_publickey_failure(&s, wire, n, 1);
        snapshot(&s, 9, methods, booleans[i] != 0);
        frankenterm_record_publickey_failure(&s, wire, n, 0);
        assert(libssh2_frankenterm_publickey_unsigned_refusal(&s) == (booleans[i] ? 0 : 9));
        none(&s);
    }
    size_t n = packet(wire, "", 0, 1);
    frankenterm_record_publickey_failure(&s, wire, n, 1);
    snapshot(&s, 9, "", 1);
}
static void wire_extents(void) {
    LIBSSH2_SESSION s = {0}; s.ft_auth_attempt = 1;
    unsigned char wire[1032]; char largest[1026];
    size_t n = packet(wire, "keyboard-interactive", 20, 1);
    for(size_t truncated = 0; truncated < n; truncated++) {
        frankenterm_record_publickey_failure(&s, wire, truncated, 1); none(&s);
    }
    frankenterm_record_publickey_failure(&s, wire, n + 1, 1); none(&s);
    frankenterm_record_publickey_failure(&s, wire, SIZE_MAX, 1); none(&s);
    frankenterm_record_publickey_failure(&s, NULL, n, 1); none(&s);
    wire[0] = SSH_MSG_USERAUTH_SUCCESS;
    frankenterm_record_publickey_failure(&s, wire, n, 1); none(&s);
    wire[0] = SSH_MSG_USERAUTH_FAILURE; memset(wire + 1, 255, 4);
    frankenterm_record_publickey_failure(&s, wire, 6, 1); none(&s);
    memset(largest, 'x', sizeof(largest)); largest[1024] = 0;
    n = packet(wire, largest, 1024, 1);
    frankenterm_record_publickey_failure(&s, wire, n, 1); snapshot(&s, 1, largest, 1);
    largest[1024] = 'x'; n = packet(wire, largest, 1025, 1);
    frankenterm_record_publickey_failure(&s, wire, n, 1); none(&s);
    wire[n - 1] = 0;
    frankenterm_record_publickey_failure(&s, wire, n, 0); none(&s);
}
static void malformed_lists_clear_old_evidence(void) {
    const char *bad[] = {",keyboard-interactive", "keyboard-interactive,",
        "publickey,,keyboard-interactive", "keyboard interactive",
        "keyboard\ninteractive", "keyboard\177interactive", "keyboard\200interactive"};
    LIBSSH2_SESSION s = {0}; s.ft_auth_attempt = 3;
    unsigned char good[64], wire[128];
    size_t good_n = packet(good, "keyboard-interactive", 20, 1);
    for(size_t i = 0; i <= sizeof(bad) / sizeof(bad[0]); i++) {
        for(int signed_request = 0; signed_request <= 1; signed_request++) {
            frankenterm_record_publickey_failure(&s, good, good_n, 1);
            size_t n;
            if(i == sizeof(bad) / sizeof(bad[0])) {
                n = packet(wire, "keyboard-interactive", 20, 0); wire[13] = 0;
            } else n = packet(wire, bad[i], strlen(bad[i]), 0);
            frankenterm_record_publickey_failure(&s, wire, n, signed_request); none(&s);
        }
    }
}
static void consuming_copy_and_arguments(void) {
    LIBSSH2_SESSION s = {0}; s.ft_auth_attempt = 11;
    unsigned char wire[64]; size_t n = packet(wire, "keyboard-interactive", 20, 1);
    libssh2_uint64_t attempt = 0; int partial = 0; size_t length = 0;
    struct { char methods[20]; unsigned char sentinel; } out;
    memset(&out, 0xa5, sizeof(out));
    frankenterm_record_publickey_failure(&s, wire, n, 1); s.err_code = -4321;
    assert(libssh2_frankenterm_publickey_failure(&s, &attempt, &partial,
        out.methods, sizeof(out.methods), &length) == LIBSSH2_ERROR_BUFFER_TOO_SMALL);
    assert(attempt == 11 && partial == 1 && length == 20 && out.methods[0] == 0);
    assert(out.sentinel == 0xa5 && s.err_code == -4321); none(&s);
    frankenterm_record_publickey_failure(&s, wire, n, 1);
    assert(libssh2_frankenterm_publickey_failure(&s, NULL, &partial,
        out.methods, sizeof(out.methods), &length) == LIBSSH2_ERROR_INVAL);
    snapshot(&s, 11, "keyboard-interactive", 1);
    assert(libssh2_frankenterm_publickey_attempt(NULL) == 0);
    assert(libssh2_frankenterm_publickey_unsigned_refusal(NULL) == 0);
    assert(libssh2_frankenterm_auth_evidence_version() == 1);
}
static void actual_attempt_freshness_and_exhaustion(void) {
    LIBSSH2_SESSION s = {0}; unsigned char wire[64];
    size_t n = packet(wire, "keyboard-interactive", 20, 1);
    frankenterm_record_publickey_failure(&s, wire, n, 1); none(&s);
    for(libssh2_uint64_t attempt = 1; attempt <= 3; attempt++) {
        /* Actual upstream attempt entry, with an early failure before any
         * allocation, network activity, or signing callback invocation. */
        assert(_libssh2_userauth_publickey(&s, "operator", 8, NULL, 0, NULL, NULL)
            == LIBSSH2_ERROR_PUBLICKEY_UNVERIFIED);
        assert(libssh2_frankenterm_publickey_attempt(&s) == attempt); none(&s);
        frankenterm_record_publickey_failure(&s, wire, n, 1);
        assert(s.ft_auth_failure_attempt == attempt);
    }
    s.ft_auth_attempt = ~(libssh2_uint64_t)0;
    assert(_libssh2_userauth_publickey(&s, "operator", 8, NULL, 0, NULL, NULL)
        == LIBSSH2_ERROR_PUBLICKEY_UNVERIFIED);
    assert(s.ft_auth_attempt_exhausted && s.ft_auth_attempt == ~(libssh2_uint64_t)0);
    frankenterm_record_publickey_failure(&s, wire, n, 1); none(&s);
    wire[n - 1] = 0;
    frankenterm_record_publickey_failure(&s, wire, n, 0); none(&s);
}
int main(void) {
    signed_and_unsigned(); wire_extents(); malformed_lists_clear_old_evidence();
    consuming_copy_and_arguments(); actual_attempt_freshness_and_exhaustion();
    puts("PASS: 5 actual-backend evidence groups (bounds, origins, consumption, freshness, exhaustion)");
    return 0;
}
