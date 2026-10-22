/* Exercises the production parser/classifier directly. Real SSH messages,
 * signing, partial authentication and transport loss are in test_native.py. */
#include <assert.h>
#include <stdio.h>
#include "../../Sources/CFrankenTermSSH/FrankenTermSSH.c"

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
        assert(publickey_method(accepted[i]) == FTSSH_OK);
    const char *unsupported[] = {
        "", "password", "not-publickey", "publickey-extra", "xpublickey",
        "PUBLICKEY", "publickey@server.example", "password,not-publickey"
    };
    for(size_t i = 0; i < sizeof(unsupported) / sizeof(unsupported[0]); i++)
        assert(publickey_method(unsupported[i]) == FTSSH_UNSUPPORTED);
    const char *malformed[] = {
        ",publickey", "publickey,", "publickey,,password", "publickey,password\n",
        "publickey, password", "publickey,\x7f", "publickey,\x80"
    };
    for(size_t i = 0; i < sizeof(malformed) / sizeof(malformed[0]); i++)
        assert(publickey_method(malformed[i]) == FTSSH_AUTH_FAILED);
    assert(publickey_method(NULL) == FTSSH_AUTH_FAILED);
    char at_limit[1025], over_limit[1026];
    memset(at_limit, 'x', sizeof(at_limit));
    memcpy(at_limit, "publickey,", 10); at_limit[1024] = 0;
    assert(publickey_method(at_limit) == FTSSH_OK);
    memset(over_limit, 'x', sizeof(over_limit));
    memcpy(over_limit, "publickey,", 10); over_limit[1025] = 0;
    assert(publickey_method(over_limit) == FTSSH_LIMIT);
}

static void test_refusal_requires_unsigned_offer_evidence(void) {
    ftssh_session session = authentication();
    assert(authentication_result(&session, LIBSSH2_ERROR_AUTHENTICATION_FAILED, 0, 1) == FTSSH_AUTH_REJECTED);
    assert(session.phase == FAILED && session.terminal_error == FTSSH_AUTH_REJECTED);
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

int main(void) {
    test_method_tokens();
    test_refusal_requires_unsigned_offer_evidence();
    test_non_refusal_failures_and_callback_evidence();
    test_progress_and_success_require_signature();
    puts("authentication boundary assertions passed");
    return 0;
}
