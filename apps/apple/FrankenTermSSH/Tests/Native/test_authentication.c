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

typedef struct { uint8_t bytes[FTSSH_CHALLENGE_CAPACITY + 1]; size_t length; } test_wire;
static void test_string(test_wire *wire, const void *bytes, size_t length) {
    assert(length <= sizeof(wire->bytes) - wire->length - 4);
    store_u32(wire->bytes + wire->length, (uint32_t)length); wire->length += 4;
    if(length) memcpy(wire->bytes + wire->length, bytes, length);
    wire->length += length;
}
static void test_name(test_wire *wire, const char *name) { test_string(wire, name, strlen(name)); }
static void test_uint(test_wire *wire, uint64_t value, size_t length) {
    assert(length <= sizeof(wire->bytes) - wire->length);
    for(size_t i = length; i; i--) wire->bytes[wire->length++] = (uint8_t)(value >> ((i - 1) * 8));
}
/* Structural fixtures deliberately omit cryptographic issuer proof: that is
 * verified by the workspace parser's independent ssh-keygen fixtures. These
 * tests exercise the transport's offer/request/signature envelope boundary. */
static test_wire test_certificate_validity(const char *algorithm, uint32_t kind, int critical, uint64_t after, uint64_t before) {
    test_wire wire = {0}, principals = {0}, extensions = {0}, authority = {0}, signature = {0};
    char name[65]; snprintf(name, sizeof(name), "%s-cert-v01@openssh.com", algorithm);
    test_name(&wire, name);
    uint8_t bytes[1025]; memset(bytes, 7, sizeof(bytes));
    test_string(&wire, bytes, 32);
    if(!strcmp(algorithm, "ssh-rsa")) {
        const uint8_t exponent[] = {1, 0, 1};
        test_string(&wire, exponent, sizeof(exponent));
        bytes[0] = 0; bytes[1] = 128; test_string(&wire, bytes, 257);
    } else if(!strcmp(algorithm, "ssh-ed25519")) { test_string(&wire, bytes, 32); }
    else {
        test_name(&wire, algorithm + 11); bytes[0] = 4;
        test_string(&wire, bytes, algorithm[16] == '2' ? 65 : (algorithm[16] == '3' ? 97 : 133));
    }
    test_uint(&wire, 19, 8); test_uint(&wire, kind, 4);
    test_name(&wire, "native-certificate-boundary");
    test_name(&principals, "operator"); test_string(&wire, principals.bytes, principals.length);
    test_uint(&wire, after, 8); test_uint(&wire, before, 8);
    if(critical) test_name(&wire, "unqualified"); else test_string(&wire, NULL, 0);
    test_name(&extensions, "permit-port-forwarding"); test_string(&extensions, NULL, 0);
    test_name(&extensions, "permit-pty"); test_string(&extensions, NULL, 0);
    test_string(&wire, extensions.bytes, extensions.length); test_string(&wire, NULL, 0);
    test_name(&authority, "ssh-ed25519"); test_string(&authority, bytes, 32);
    test_string(&wire, authority.bytes, authority.length);
    test_name(&signature, "ssh-ed25519"); test_string(&signature, bytes, 64);
    test_string(&wire, signature.bytes, signature.length);
    return wire;
}
static test_wire test_certificate(const char *algorithm, uint32_t kind, int critical) {
    return test_certificate_validity(algorithm, kind, critical, 1, UINT64_MAX);
}
static ftssh_session test_certificate_session(const test_wire *wire) {
    ftssh_session session = authentication(); session.limit = 65536;
    uint64_t validity[2];
    assert(public_key_profile(wire->bytes, wire->length, 1, session.algorithm, session.offer_algorithm,
        &session.rsa_signature_length, &session.user_certificate, validity) == FTSSH_OK);
    assert(session.user_certificate);
    memcpy(session.user, "operator", 9);
    memcpy(session.public_blob, wire->bytes, wire->length); session.public_length = wire->length;
    return session;
}
static test_wire test_certificate_challenge(const test_wire *certificate, const char *algorithm) {
    test_wire wire = {0}; uint8_t session_identifier[32] = {1};
    test_string(&wire, session_identifier, sizeof(session_identifier)); test_uint(&wire, 50, 1);
    test_name(&wire, "operator"); test_name(&wire, "ssh-connection"); test_name(&wire, "publickey"); test_uint(&wire, 1, 1);
    test_name(&wire, algorithm); test_string(&wire, certificate->bytes, certificate->length);
    return wire;
}
static void test_user_certificate_profiles_and_bounds(void) {
    test_wire certificate = test_certificate("ssh-ed25519", 1, 0);
    char algorithm[65], offer[65]; size_t rsa_length; int is_certificate;
    uint64_t validity[2];
    assert(public_key_profile(certificate.bytes, certificate.length, 1, algorithm, offer, &rsa_length, &is_certificate, validity) == FTSSH_OK);
    assert(!strcmp(algorithm, "ssh-ed25519") && !strcmp(offer, "ssh-ed25519-cert-v01@openssh.com"));
    assert(is_certificate && !rsa_length && validity[0] == 1 && validity[1] == UINT64_MAX);
    assert(public_key_profile(certificate.bytes, certificate.length, 0, algorithm, offer, &rsa_length, &is_certificate, validity) == FTSSH_UNSUPPORTED);
    for(size_t i = 0; i < certificate.length; i++)
        assert(public_key_profile(certificate.bytes, i, 1, algorithm, offer, &rsa_length, &is_certificate, validity) != FTSSH_OK);
    assert(public_key_profile(certificate.bytes, certificate.length + 1, 1, algorithm, offer, &rsa_length, &is_certificate, validity) != FTSSH_OK);
    assert(public_key_profile(certificate.bytes, FTSSH_PUBLIC_KEY_CAPACITY + 1, 1, algorithm, offer, &rsa_length, &is_certificate, validity) == FTSSH_INVALID);
    certificate = test_certificate("ssh-ed25519", 2, 0);
    assert(public_key_profile(certificate.bytes, certificate.length, 1, algorithm, offer, &rsa_length, &is_certificate, validity) == FTSSH_INVALID);
    certificate = test_certificate("ssh-ed25519", 1, 1);
    assert(public_key_profile(certificate.bytes, certificate.length, 1, algorithm, offer, &rsa_length, &is_certificate, validity) == FTSSH_UNSUPPORTED);
}
static void test_certificate_offer_time_rechecks_after_identity_selection(void) {
    const uint64_t validity[] = {100, 200};
    assert(!certificate_time_valid(validity, (time_t)-1));
    assert(!certificate_time_valid(validity, (time_t)-2));
    assert(!certificate_time_valid(validity, 99));
    assert(certificate_time_valid(validity, 100));
    assert(certificate_time_valid(validity, 199));
    assert(!certificate_time_valid(validity, 200));
    test_wire expired = test_certificate_validity("ssh-ed25519", 1, 0, 0, 1);
    test_wire future = test_certificate_validity("ssh-ed25519", 1, 0, UINT64_MAX - 1, UINT64_MAX);
    ftssh_session session = {0}; session.phase = APPROVED;
    assert(ftssh_begin_authentication(&session, "operator", 8, expired.bytes, expired.length) == FTSSH_AUTH_FAILED);
    assert(session.phase == APPROVED && !session.public_length && !session.challenge_length);
    assert(ftssh_begin_authentication(&session, "operator", 8, future.bytes, future.length) == FTSSH_AUTH_FAILED);
    assert(session.phase == APPROVED && !session.public_length && !session.challenge_length);
}
static test_wire test_certificate_rsa_authority(size_t signature_length) {
    test_wire certificate = test_certificate("ssh-ed25519", 1, 0), authority = {0}, envelope = {0};
    const uint8_t *cursor = certificate.bytes, *value; size_t left = certificate.length, length;
    /* Retain the certificate subject and policy, then replace its CA envelope.
     * The production parser has no role in manufacturing these wire fields. */
    for(int i = 0; i < 3; i++) assert(field(&cursor, &left, &value, &length));
    assert(left >= 12); cursor += 12; left -= 12;
    for(int i = 0; i < 2; i++) assert(field(&cursor, &left, &value, &length));
    assert(left >= 16); cursor += 16; left -= 16;
    for(int i = 0; i < 3; i++) assert(field(&cursor, &left, &value, &length));
    certificate.length = (size_t)(cursor - certificate.bytes);
    const uint8_t exponent[] = {1, 0, 1}; uint8_t modulus[257]; memset(modulus, 7, sizeof(modulus));
    modulus[0] = 0; modulus[1] = 128;
    test_name(&authority, "ssh-rsa"); test_string(&authority, exponent, sizeof(exponent));
    test_string(&authority, modulus, sizeof(modulus));
    test_string(&certificate, authority.bytes, authority.length);
    uint8_t signature[257]; memset(signature, 7, sizeof(signature));
    assert(signature_length <= sizeof(signature));
    test_name(&envelope, "rsa-sha2-512"); test_string(&envelope, signature, signature_length);
    test_string(&certificate, envelope.bytes, envelope.length);
    return certificate;
}
static void test_certificate_rsa_authority_allows_only_bounded_short_signatures(void) {
    const struct { size_t length; int expected; } cases[] = {
        {1, FTSSH_OK}, {255, FTSSH_OK}, {256, FTSSH_OK}, {0, FTSSH_INVALID}, {257, FTSSH_INVALID}
    };
    char algorithm[65], offer[65]; size_t rsa_length; int is_certificate; uint64_t validity[2];
    for(size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        test_wire certificate = test_certificate_rsa_authority(cases[i].length);
        assert(public_key_profile(certificate.bytes, certificate.length, 1, algorithm, offer,
            &rsa_length, &is_certificate, validity) == cases[i].expected);
    }
}
static void test_certificate_request_and_signature_algorithms_are_distinct(void) {
    const char *subjects[] = {"ssh-ed25519", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521", "ssh-rsa", "ssh-rsa"};
    const char *signatures[] = {"ssh-ed25519", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521", "rsa-sha2-256", "rsa-sha2-512"};
    for(size_t i = 0; i < sizeof(subjects) / sizeof(subjects[0]); i++) {
        test_wire certificate = test_certificate(subjects[i], 1, 0);
        ftssh_session session = test_certificate_session(&certificate);
        char request[65]; snprintf(request, sizeof(request), "%s-cert-v01@openssh.com", signatures[i]);
        test_wire challenge = test_certificate_challenge(&certificate, request);
        void *context = &session; unsigned char *signature = NULL; size_t length = 0;
        assert(sign_callback(NULL, &signature, &length, challenge.bytes, challenge.length, &context) == LIBSSH2_ERROR_EAGAIN);
        assert(!strcmp(session.signature_algorithm, signatures[i]));
        assert(session.challenge_length == challenge.length && !memcmp(session.challenge, challenge.bytes, challenge.length));
        test_wire envelope = {0}, payload = {0}; uint8_t bytes[256]; memset(bytes, 7, sizeof(bytes));
        if(session.rsa_signature_length) { memcpy(payload.bytes, bytes, sizeof(bytes)); payload.length = sizeof(bytes); }
        else if(!strcmp(subjects[i], "ssh-ed25519")) { memcpy(payload.bytes, bytes, 64); payload.length = 64; }
        else { test_string(&payload, bytes, 1); test_string(&payload, bytes, 1); }
        test_name(&envelope, request); test_string(&envelope, payload.bytes, payload.length);
        assert(ftssh_supply_signature(&session, envelope.bytes, envelope.length) == FTSSH_CHALLENGE);
        if(session.rsa_signature_length) {
            /* A short certificate-issuer envelope never relaxes the exact
             * modulus-width response required from our protected signer. */
            envelope.length = 0; test_name(&envelope, signatures[i]);
            test_string(&envelope, payload.bytes, payload.length - 1);
            assert(ftssh_supply_signature(&session, envelope.bytes, envelope.length) == FTSSH_CHALLENGE);
        }
        envelope.length = 0; test_name(&envelope, signatures[i]); test_string(&envelope, payload.bytes, payload.length);
        assert(ftssh_supply_signature(&session, envelope.bytes, envelope.length) == FTSSH_OK);
        assert(sign_callback(NULL, &signature, &length, challenge.bytes, challenge.length, &context) == 0);
        assert(length == payload.length && !memcmp(signature, payload.bytes, length));
        release(signature, &context); assert(!session.allocated);

        session = test_certificate_session(&certificate);
        challenge = test_certificate_challenge(&certificate, signatures[i]);
        assert(sign_callback(NULL, &signature, &length, challenge.bytes, challenge.length, &context) == LIBSSH2_ERROR_INVAL);
        assert(!session.challenge_length && !signature);
        session = test_certificate_session(&certificate);
        challenge = test_certificate_challenge(&certificate, request); challenge.bytes[challenge.length - 1] ^= 1;
        assert(sign_callback(NULL, &signature, &length, challenge.bytes, challenge.length, &context) == LIBSSH2_ERROR_INVAL);
        assert(!session.challenge_length && !signature);
    }
    test_wire rsa = test_certificate("ssh-rsa", 1, 0);
    ftssh_session session = test_certificate_session(&rsa);
    test_wire challenge = test_certificate_challenge(&rsa, "ssh-rsa-cert-v01@openssh.com");
    void *context = &session; unsigned char *signature = NULL; size_t length = 0;
    assert(sign_callback(NULL, &signature, &length, challenge.bytes, challenge.length, &context) == LIBSSH2_ERROR_INVAL);
    assert(session.sign_error == FTSSH_UNSUPPORTED && !session.challenge_length && !signature);
}

int main(void) {
    test_authentication_allocations_are_erased_before_free();
    test_method_tokens();
    test_unproven_unsigned_failure_is_terminal();
    test_non_refusal_failures_and_callback_evidence();
    test_progress_and_success_require_signature();
    test_combined_deadline_and_partial_phase_are_native_boundaries();
    test_user_certificate_profiles_and_bounds();
    test_certificate_offer_time_rechecks_after_identity_selection();
    test_certificate_rsa_authority_allows_only_bounded_short_signatures();
    test_certificate_request_and_signature_algorithms_are_distinct();
    puts("authentication boundary assertions passed");
    return 0;
}
