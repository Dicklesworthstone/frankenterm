/* Exercises the production parser/classifier directly. Real SSH messages,
 * signing, partial authentication and transport loss are in test_native.py. */
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
static void observed_free(void *pointer);
#define free observed_free
#include "../../Sources/CFrankenTermSSH/FrankenTermSSH.c"
#undef free

static void *watched_allocation;
static size_t watched_size;
static unsigned int wiped_releases;
static void observed_free(void *pointer) {
    if(pointer && pointer == watched_allocation) {
        const uint8_t *payload = (const uint8_t *)((allocation_header *)pointer + 1);
        for(size_t i = 0; i < watched_size; i++) assert(payload[i] == 0);
        watched_allocation = NULL;
        wiped_releases++;
    }
    free(pointer);
}

static void test_authentication_allocations_are_erased_before_free(void) {
    ftssh_session session = {0};
    session.phase = AUTH; session.limit = 65536;
    void *context = &session;
    uint8_t *original = allocate(32, &context);
    assert(original);
    memset(original, 0xa5, 32);
    /* Changing phases cannot discard the original allocation's sensitivity. */
    session.phase = LIVE;
    watched_allocation = (allocation_header *)original - 1; watched_size = 32;
    uint8_t *replacement = resize_allocation(original, 64, &context);
    assert(replacement && wiped_releases == 1 && !watched_allocation);
    for(size_t i = 0; i < 32; i++) assert(replacement[i] == 0xa5);
    memset(replacement + 32, 0x5a, 32);
    watched_allocation = (allocation_header *)replacement - 1; watched_size = 64;
    release(replacement, &context);
    assert(wiped_releases == 2 && !watched_allocation && session.allocated == 0);
}

static ftssh_session authentication(void) {
    ftssh_session session = {0};
    session.phase = AUTH;
    session.auth_methods_checked = 1;
    return session;
}

static void test_method_tokens(void) {
    const char *accepted[] = {
        "publickey", "password,publickey", "publickey,password",
        "keyboard-interactive,publickey,password", "publickey,publickey"
    };
    for(size_t i = 0; i < sizeof(accepted) / sizeof(accepted[0]); i++)
        assert(authentication_method(accepted[i], "publickey") == FTSSH_OK);
    const char *unsupported[] = {
        "", "password", "not-publickey", "publickey-extra", "xpublickey",
        "PUBLICKEY", "publickey@server.example", "password,not-publickey"
    };
    for(size_t i = 0; i < sizeof(unsupported) / sizeof(unsupported[0]); i++)
        assert(authentication_method(unsupported[i], "publickey") == FTSSH_UNSUPPORTED);
    const char *malformed[] = {
        ",publickey", "publickey,", "publickey,,password", "publickey,password\n",
        "publickey, password", "publickey,\x7f", "publickey,\x80"
    };
    for(size_t i = 0; i < sizeof(malformed) / sizeof(malformed[0]); i++)
        assert(authentication_method(malformed[i], "publickey") == FTSSH_AUTH_FAILED);
    assert(authentication_method(NULL, "publickey") == FTSSH_AUTH_FAILED);
    char at_limit[1025], over_limit[1026];
    memset(at_limit, 'x', sizeof(at_limit));
    memcpy(at_limit, "publickey,", 10); at_limit[1024] = 0;
    assert(authentication_method(at_limit, "publickey") == FTSSH_OK);
    memset(over_limit, 'x', sizeof(over_limit));
    memcpy(over_limit, "publickey,", 10); over_limit[1025] = 0;
    assert(authentication_method(over_limit, "publickey") == FTSSH_LIMIT);
}

static void test_unproven_unsigned_failure_is_terminal(void) {
    ftssh_session session = authentication();
    /* A generic library return code has no proof of the partial-success byte.
     * Actual fresh unsigned-refusal evidence is tested with the real server. */
    assert(authentication_result(&session, LIBSSH2_ERROR_AUTHENTICATION_FAILED, 0, 1) == FTSSH_AUTH_FAILED);
    assert(session.phase == FAILED && session.terminal_error == FTSSH_AUTH_FAILED);
    session = authentication();
    assert(authentication_result(&session, LIBSSH2_ERROR_AUTHENTICATION_FAILED, 0, 0) == FTSSH_AUTH_FAILED);
    session = authentication(); session.auth_methods_checked = 0;
    assert(authentication_result(&session, LIBSSH2_ERROR_AUTHENTICATION_FAILED, 0, 1) == FTSSH_AUTH_FAILED);
    session = authentication(); session.sign_callback_entered = 1;
    assert(authentication_result(&session, LIBSSH2_ERROR_AUTHENTICATION_FAILED, 0, 1) == FTSSH_AUTH_FAILED);
    session = authentication(); session.challenge_length = 1;
    assert(authentication_result(&session, LIBSSH2_ERROR_AUTHENTICATION_FAILED, 0, 1) == FTSSH_AUTH_FAILED);
    session = authentication(); session.signature_consumed = 1;
    assert(authentication_result(&session, LIBSSH2_ERROR_AUTHENTICATION_FAILED, 0, 1) == FTSSH_AUTH_FAILED);
}

static void test_non_refusal_failures_and_callback_evidence(void) {
    const struct { int input, expected; } failures[] = {
        {LIBSSH2_ERROR_PUBLICKEY_UNVERIFIED, FTSSH_AUTH_FAILED},
        {LIBSSH2_ERROR_PROTO, FTSSH_AUTH_FAILED},
        {LIBSSH2_ERROR_INVALID_MAC, FTSSH_AUTH_FAILED},
        {LIBSSH2_ERROR_TIMEOUT, FTSSH_AUTH_FAILED},
        {LIBSSH2_ERROR_SOCKET_TIMEOUT, FTSSH_AUTH_FAILED},
        {LIBSSH2_ERROR_ALLOC, FTSSH_LIMIT},
        {LIBSSH2_ERROR_METHOD_NONE, FTSSH_UNSUPPORTED},
        {LIBSSH2_ERROR_METHOD_NOT_SUPPORTED, FTSSH_UNSUPPORTED},
        {LIBSSH2_ERROR_ALGO_UNSUPPORTED, FTSSH_UNSUPPORTED},
        {LIBSSH2_ERROR_SOCKET_SEND, FTSSH_IO},
        {LIBSSH2_ERROR_SOCKET_RECV, FTSSH_IO},
        {LIBSSH2_ERROR_SOCKET_DISCONNECT, FTSSH_IO},
        {LIBSSH2_ERROR_BAD_SOCKET, FTSSH_IO},
        {-9999, FTSSH_AUTH_FAILED}, {0, FTSSH_AUTH_FAILED}
    };
    for(size_t i = 0; i < sizeof(failures) / sizeof(failures[0]); i++) {
        ftssh_session session = authentication();
        assert(authentication_result(&session, failures[i].input, 0, 1) == failures[i].expected);
        assert(session.phase == FAILED && session.terminal_error != FTSSH_AUTH_REJECTED);
    }
    ftssh_session session = authentication(); session.socket_failed = 1;
    assert(authentication_result(&session, LIBSSH2_ERROR_PUBLICKEY_UNVERIFIED, 0, 1) == FTSSH_IO);
    session = authentication(); session.socket_failed = 1;
    assert(authentication_result(&session, LIBSSH2_ERROR_AUTHENTICATION_FAILED, 0, 1) == FTSSH_IO);
    session = authentication(); session.sign_error = FTSSH_LIMIT;
    assert(authentication_result(&session, LIBSSH2_ERROR_PUBLICKEY_UNVERIFIED, 0, 1) == FTSSH_LIMIT);

    session = authentication();
    void *context = &session;
    unsigned char *signature = NULL, malformed[] = {0};
    size_t signature_length = 0;
    assert(sign_callback(NULL, &signature, &signature_length, malformed, sizeof(malformed), &context) == LIBSSH2_ERROR_INVAL);
    assert(session.sign_callback_entered && !session.challenge_length && !signature);
    assert(authentication_result(&session, LIBSSH2_ERROR_PUBLICKEY_UNVERIFIED, 0, 1) == FTSSH_CHALLENGE);
}

static void test_progress_and_success_require_signature(void) {
    ftssh_session session = authentication();
    assert(authentication_result(&session, LIBSSH2_ERROR_EAGAIN, 0, 1) == FTSSH_AGAIN);
    assert(session.phase == AUTH);
    session.challenge_length = 32; session.sign_callback_entered = 1;
    assert(authentication_result(&session, LIBSSH2_ERROR_EAGAIN, 0, 1) == FTSSH_SIGNATURE_NEEDED);
    session = authentication();
    assert(authentication_result(&session, 0, 1, 0) == FTSSH_UNSUPPORTED);
    session = authentication();
    assert(authentication_result(&session, LIBSSH2_ERROR_EAGAIN, 1, 0) == FTSSH_UNSUPPORTED);
    session = authentication();
    assert(authentication_result(&session, LIBSSH2_ERROR_EAGAIN, 1, 1) == FTSSH_UNSUPPORTED);
    session = authentication();
    assert(authentication_result(&session, 0, 1, 1) == FTSSH_UNSUPPORTED);
    session = authentication(); session.sign_callback_entered = 1;
    assert(authentication_result(&session, 0, 1, 1) == FTSSH_UNSUPPORTED);
    session = authentication(); session.sign_callback_entered = 1; session.signature_consumed = 1;
    assert(authentication_result(&session, 0, 1, 1) == FTSSH_OK);
    assert(session.phase == AUTHENTICATED);
}

static void test_combined_deadline_and_partial_phase_are_native_boundaries(void) {
    ftssh_session session = authentication();
    session.publickey_interactive = 1; session.authentication_deadline = 1;
    assert(ftssh_authentication_step(&session) == FTSSH_AUTH_TIMEOUT);
    assert(session.phase == FAILED);
    session = authentication(); session.publickey_interactive = 1; session.authentication_deadline = 1;
    uint8_t bytes[16] = {0}; size_t length = 0;
    assert(ftssh_copy_challenge(&session, bytes, sizeof(bytes), &length) == FTSSH_AUTH_TIMEOUT);
    session = authentication(); session.publickey_interactive = 1; session.authentication_deadline = 1;
    assert(ftssh_supply_signature(&session, bytes, sizeof(bytes)) == FTSSH_AUTH_TIMEOUT);
    session = authentication(); session.publickey_interactive = 1; session.authentication_deadline = 1;
    session.sign_callback_entered = session.signature_consumed = 1;
    assert(authentication_result(&session, 0, 1, 1) == FTSSH_AUTH_TIMEOUT);

    session = authentication(); session.phase = PARTIAL_AUTH;
    assert(ftssh_continue_interactive_authentication(&session) == FTSSH_STATE);
    assert(ftssh_begin_shell(&session, 80, 24, 0, 0) == FTSSH_STATE);
    assert(ftssh_begin_forward(&session, "target", 6, 22) == FTSSH_STATE);
    assert(ftssh_begin_password_authentication(&session, "operator", 8) == FTSSH_STATE);
    assert(ftssh_begin_interactive_authentication(&session, "operator", 8, 3000) == FTSSH_STATE);
    session.publickey_interactive = session.partial_ready = 1;
    session.authentication_deadline = 1;
    assert(ftssh_continue_interactive_authentication(&session) == FTSSH_AUTH_TIMEOUT);
    assert(!session.partial_ready && session.phase == FAILED);

    session = authentication(); session.phase = PARTIAL_AUTH;
    session.publickey_interactive = session.partial_ready = 1;
    session.authentication_deadline = monotonic_milliseconds() + 3000;
    memcpy(session.user, "operator", 9);
    unsigned int workers = atomic_load(&global_interactive_workers);
    /* No allocation budget: continuation must consume its proof, remain
     * unauthenticated, and leave no worker or global admission charge. */
    assert(ftssh_continue_interactive_authentication(&session) == FTSSH_LIMIT);
    assert(session.phase == FAILED && !session.partial_ready && !session.interactive);
    assert(atomic_load(&global_interactive_workers) == workers);
}

int main(void) {
    test_authentication_allocations_are_erased_before_free();
    test_method_tokens();
    test_unproven_unsigned_failure_is_terminal();
    test_non_refusal_failures_and_callback_evidence();
    test_progress_and_success_require_signature();
    test_combined_deadline_and_partial_phase_are_native_boundaries();
    puts("authentication boundary assertions passed");
    return 0;
}
