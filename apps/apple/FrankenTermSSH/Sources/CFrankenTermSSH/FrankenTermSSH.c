#include "FrankenTermSSH.h"
#include <libssh2.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdalign.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#if defined(__linux__)
#include <linux/sockios.h>
#include <sys/ioctl.h>
#endif

#if LIBSSH2_VERSION_NUM < 0x010b01
#error "FrankenTerm native SSH requires libssh2 1.11.1 or later"
#endif

/* Audited exception: opaque libssh2 handles, POSIX socket ownership, aligned
 * allocator headers and synchronous C callbacks are confined to this file.
 * Bounds are checked before copy/allocation. The Swift owner serializes calls.
 * Callbacks never invoke Swift, block, throw, or retain a borrowed argument.
 * The library owns callback signatures through the same supplied allocator.
 * No global signal handler, key-file loading, shell command or trace is used.
 */
enum phase { HANDSHAKE, PEER, APPROVED, AUTH, AUTHENTICATED, OPENING, PTY, SHELL, LIVE, FAILED, CLOSED };
struct ftssh_session {
    LIBSSH2_SESSION *ssh;
    LIBSSH2_CHANNEL *channel;
    int fd, phase, terminal_error, closing;
    size_t limit, allocated, peak;
    char user[257], algorithm[65];
    uint8_t public_blob[512], challenge[1024], signature[256];
    size_t public_length, challenge_length, signature_length;
    int signature_consumed, resize_pending;
    uint32_t geometry[4], resize_geometry[4];
    uint8_t write_buffer[FTSSH_WRITE_CAPACITY];
    size_t write_length, write_offset;
    uint64_t accepted, written, received;
};
typedef union { max_align_t alignment; size_t size; } allocation_header;
static _Atomic size_t global_allocated;
static _Atomic unsigned int global_sessions;
static const size_t global_limit = 16u * 1024u * 1024u;
static pthread_once_t initialization = PTHREAD_ONCE_INIT;
static int initialized;
static void initialize(void) { initialized = libssh2_init(0); }

static int reserve(size_t amount) {
    size_t previous = atomic_load(&global_allocated);
    do { if(amount > global_limit - previous) return 0; }
    while(!atomic_compare_exchange_weak(&global_allocated, &previous, previous + amount));
    return 1;
}
static void *allocate(size_t size, void **abstract) {
    ftssh_session *s = *abstract;
    if(size > SIZE_MAX - sizeof(allocation_header)) return NULL;
    size_t total = size + sizeof(allocation_header);
    if(total > s->limit - s->allocated || !reserve(total)) return NULL;
    allocation_header *h = malloc(total);
    if(!h) { atomic_fetch_sub(&global_allocated, total); return NULL; }
    h->size = total; s->allocated += total;
    if(s->allocated > s->peak) s->peak = s->allocated;
    return h + 1;
}
static void release(void *pointer, void **abstract) {
    if(!pointer) return;
    ftssh_session *s = *abstract;
    allocation_header *h = (allocation_header *)pointer - 1;
    size_t total = h->size;
    s->allocated -= total; atomic_fetch_sub(&global_allocated, total);
    free(h);
}
static void *resize_allocation(void *pointer, size_t size, void **abstract) {
    if(!pointer) return allocate(size, abstract);
    if(!size) { release(pointer, abstract); return NULL; }
    /* Reserve actual overlap rather than pretending realloc cannot duplicate. */
    allocation_header *old = (allocation_header *)pointer - 1;
    void *replacement = allocate(size, abstract);
    if(!replacement) return NULL;
    size_t previous = old->size - sizeof(allocation_header);
    memcpy(replacement, pointer, size < previous ? size : previous);
    release(pointer, abstract); return replacement;
}
static ssize_t socket_send(libssh2_socket_t fd, const void *data, size_t length, int flags, void **abstract) {
    ftssh_session *s = *abstract;
    if(s->closing) return -ECONNRESET;
#ifdef MSG_NOSIGNAL
    flags |= MSG_NOSIGNAL;
#endif
    ssize_t result = send(fd, data, length, flags);
    return result < 0 ? -errno : result;
}
static ssize_t socket_receive(libssh2_socket_t fd, void *data, size_t length, int flags, void **abstract) {
    ftssh_session *s = *abstract;
    if(s->closing) return -ECONNRESET;
    ssize_t result = recv(fd, data, length, flags);
    if(result > 0 && (uint64_t)result <= UINT64_MAX - s->received) s->received += (uint64_t)result;
    return result < 0 ? -errno : result;
}
static int fail(ftssh_session *s, int code) {
    s->phase = FAILED; s->terminal_error = code; return code;
}
static int check(ftssh_session *s, int expected) {
    if(!s) return FTSSH_INVALID;
    if(s->phase == CLOSED) return FTSSH_CLOSED;
    if(s->phase == FAILED) return s->terminal_error;
    return s->phase == expected ? FTSSH_OK : FTSSH_STATE;
}
static int equal(const uint8_t *a, const uint8_t *b, size_t n) {
    unsigned int difference = 0;
    for(size_t i = 0; i < n; i++) difference |= a[i] ^ b[i];
    return difference == 0;
}
static int field(const uint8_t **bytes, size_t *left, const uint8_t **value, size_t *length) {
    if(*left < 4) return 0;
    const uint8_t *p = *bytes;
    uint32_t n = (uint32_t)p[0] << 24 | (uint32_t)p[1] << 16 | (uint32_t)p[2] << 8 | p[3];
    if(n > *left - 4) return 0;
    *value = p + 4; *length = n; *bytes += 4 + n; *left -= 4 + n; return 1;
}
static int expected_field(const uint8_t **bytes, size_t *left, const void *expected, size_t length) {
    const uint8_t *value; size_t n;
    return field(bytes, left, &value, &n) && n == length && equal(value, expected, n);
}
static int valid_geometry(uint32_t c, uint32_t r, uint32_t w, uint32_t h) {
    return c && r && c <= 512 && r <= 512 && c * r <= 65536 && w <= 65535 && h <= 65535;
}
static int result(ftssh_session *s, int rc, int failure) {
    if(rc == LIBSSH2_ERROR_EAGAIN) return FTSSH_AGAIN;
    return rc == 0 ? FTSSH_OK : fail(s, failure);
}

int ftssh_create(int fd, size_t maximum, ftssh_session **out) {
    if(!out) return FTSSH_INVALID;
    *out = NULL;
    if(fd < 0 || maximum < 65536 || maximum > 4u * 1024u * 1024u) return FTSSH_INVALID;
    int kind = 0; socklen_t kind_length = sizeof(kind);
    struct sockaddr_storage peer; socklen_t peer_length = sizeof(peer);
    if(getsockopt(fd, SOL_SOCKET, SO_TYPE, &kind, &kind_length) || kind != SOCK_STREAM ||
       getpeername(fd, (struct sockaddr *)&peer, &peer_length)) return FTSSH_INVALID;
    int flags = fcntl(fd, F_GETFL), fd_flags = fcntl(fd, F_GETFD);
    if(flags < 0 || fd_flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0 ||
       fcntl(fd, F_SETFD, fd_flags | FD_CLOEXEC) < 0) return FTSSH_IO;
#ifdef SO_NOSIGPIPE
    int one = 1;
    if(setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof(one))) return FTSSH_IO;
#endif
    pthread_once(&initialization, initialize);
    if(initialized != 0 || !libssh2_version(0x010b01)) return FTSSH_UNSUPPORTED;
    unsigned int count = atomic_load(&global_sessions);
    do { if(count >= 32) return FTSSH_LIMIT; }
    while(!atomic_compare_exchange_weak(&global_sessions, &count, count + 1));
    ftssh_session *s = calloc(1, sizeof(*s));
    if(!s) { atomic_fetch_sub(&global_sessions, 1); return FTSSH_LIMIT; }
    s->fd = fd; s->limit = maximum; s->phase = HANDSHAKE;
    s->ssh = libssh2_session_init_ex(allocate, release, resize_allocation, s);
    if(!s->ssh) { free(s); atomic_fetch_sub(&global_sessions, 1); return FTSSH_LIMIT; }
    libssh2_session_set_blocking(s->ssh, 0);
    libssh2_session_callback_set2(s->ssh, LIBSSH2_CALLBACK_SEND, (libssh2_cb_generic *)socket_send);
    libssh2_session_callback_set2(s->ssh, LIBSSH2_CALLBACK_RECV, (libssh2_cb_generic *)socket_receive);
    const struct { int method; const char *allowed; } policies[] = {
        {LIBSSH2_METHOD_KEX, "curve25519-sha256,curve25519-sha256@libssh.org,ecdh-sha2-nistp256,ecdh-sha2-nistp384,ecdh-sha2-nistp521"},
        {LIBSSH2_METHOD_HOSTKEY, "ssh-ed25519,ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,ecdsa-sha2-nistp521,rsa-sha2-512,rsa-sha2-256"},
        {LIBSSH2_METHOD_CRYPT_CS, "aes256-ctr,aes128-ctr"}, {LIBSSH2_METHOD_CRYPT_SC, "aes256-ctr,aes128-ctr"},
        {LIBSSH2_METHOD_MAC_CS, "hmac-sha2-256-etm@openssh.com,hmac-sha2-512-etm@openssh.com"},
        {LIBSSH2_METHOD_MAC_SC, "hmac-sha2-256-etm@openssh.com,hmac-sha2-512-etm@openssh.com"},
        {LIBSSH2_METHOD_COMP_CS, "none"}, {LIBSSH2_METHOD_COMP_SC, "none"}
    };
    for(size_t i = 0; i < sizeof(policies)/sizeof(policies[0]); i++) {
        if(libssh2_session_method_pref(s->ssh, policies[i].method, policies[i].allowed)) {
            /* Creation has not taken ownership of the caller's fd yet. */
            s->closing = 1; libssh2_session_free(s->ssh); free(s);
            atomic_fetch_sub(&global_sessions, 1); return FTSSH_UNSUPPORTED;
        }
    }
    *out = s; return FTSSH_OK;
}
int ftssh_handshake(ftssh_session *s) {
    int rc = check(s, HANDSHAKE); if(rc) return rc;
    rc = result(s, libssh2_session_handshake(s->ssh, s->fd), FTSSH_NEGOTIATION);
    if(rc == FTSSH_OK) s->phase = PEER;
    return rc;
}
int ftssh_copy_host_key(ftssh_session *s, uint8_t *out, size_t capacity, size_t *length) {
    if(!s || !out || !length) return FTSSH_INVALID;
    *length = 0;
    if(s->phase < PEER || s->phase >= FAILED) return FTSSH_STATE;
    size_t n = 0; int type = 0;
    const char *key = libssh2_session_hostkey(s->ssh, &n, &type);
    if(!key || !n || n > 65536) return fail(s, FTSSH_NEGOTIATION);
    if(n > capacity) return FTSSH_LIMIT;
    memcpy(out, key, n); *length = n; return FTSSH_OK;
}
int ftssh_copy_method(ftssh_session *s, int method, char *out, size_t capacity) {
    if(!s || !out || !capacity || method < 0 || method > LIBSSH2_METHOD_COMP_SC) return FTSSH_INVALID;
    if(s->phase < PEER || s->phase >= FAILED) return FTSSH_STATE;
    const char *value = libssh2_session_methods(s->ssh, method);
    if(!value) return FTSSH_UNSUPPORTED;
    size_t n = strnlen(value, 128);
    if(n >= 128 || n >= capacity) return FTSSH_LIMIT;
    memcpy(out, value, n + 1); return FTSSH_OK;
}
int ftssh_approve_host_key(ftssh_session *s, const uint8_t *key, size_t length) {
    int rc = check(s, PEER); if(rc) return rc;
    if(!key || !length || length > 65536) return FTSSH_INVALID;
    size_t n; int type;
    const char *actual = libssh2_session_hostkey(s->ssh, &n, &type);
    if(!actual || n != length || !equal((const uint8_t *)actual, key, n)) return fail(s, FTSSH_HOST_REJECTED);
    s->phase = APPROVED; return FTSSH_OK;
}
int ftssh_begin_authentication(ftssh_session *s, const char *user, size_t user_length,
                              const uint8_t *blob, size_t length) {
    int rc = check(s, APPROVED); if(rc) return rc;
    if(!user || !user_length || user_length > 256 || !blob || !length || length > 512) return FTSSH_INVALID;
    for(size_t i = 0; i < user_length; i++) if((unsigned char)user[i] < 32 || user[i] == 127) return FTSSH_INVALID;
    const uint8_t *cursor = blob, *algorithm; size_t left = length, n;
    if(!field(&cursor, &left, &algorithm, &n) || n >= sizeof(s->algorithm)) return FTSSH_INVALID;
    if(!((n == 11 && equal(algorithm, (const uint8_t *)"ssh-ed25519", n)) ||
         (n == 19 && (equal(algorithm, (const uint8_t *)"ecdsa-sha2-nistp256", n) ||
                      equal(algorithm, (const uint8_t *)"ecdsa-sha2-nistp384", n) ||
                      equal(algorithm, (const uint8_t *)"ecdsa-sha2-nistp521", n))))) return FTSSH_UNSUPPORTED;
    const uint8_t *point; size_t point_length;
    if(n == 11) {
        if(!field(&cursor, &left, &point, &point_length) || point_length != 32 || left) return FTSSH_INVALID;
    } else {
        /* Curve and uncompressed point length must agree with the algorithm.
         * Mathematical point validation remains with libssh2 and the signer. */
        if(!expected_field(&cursor, &left, algorithm + 11, 8) ||
           !field(&cursor, &left, &point, &point_length) || left) return FTSSH_INVALID;
        size_t width = algorithm[16] == '2' ? 65 : (algorithm[16] == '3' ? 97 : 133);
        if(point_length != width || point[0] != 4) return FTSSH_INVALID;
    }
    memcpy(s->algorithm, algorithm, n); s->algorithm[n] = 0;
    memcpy(s->user, user, user_length); s->user[user_length] = 0;
    memcpy(s->public_blob, blob, length); s->public_length = length;
    s->phase = AUTH; return FTSSH_OK;
}
static int sign_callback(LIBSSH2_SESSION *ssh, unsigned char **signature, size_t *signature_length,
                         const unsigned char *data, size_t length, void **abstract) {
    (void)ssh;
    ftssh_session *s = *abstract;
    *signature = NULL; *signature_length = 0;
    const uint8_t *cursor = data, *sid; size_t left = length, sid_length;
    if(length > sizeof(s->challenge) || !field(&cursor, &left, &sid, &sid_length) ||
       !(sid_length == 32 || sid_length == 48 || sid_length == 64) || !left || *cursor++ != 50) return -1;
    left--;
    if(!expected_field(&cursor, &left, s->user, strlen(s->user)) ||
       !expected_field(&cursor, &left, "ssh-connection", 14) ||
       !expected_field(&cursor, &left, "publickey", 9) || !left || *cursor++ != 1) return -1;
    left--;
    if(!expected_field(&cursor, &left, s->algorithm, strlen(s->algorithm)) ||
       !expected_field(&cursor, &left, s->public_blob, s->public_length) || left) return -1;
    if(s->challenge_length && (s->challenge_length != length || !equal(s->challenge, data, length))) return -1;
    if(!s->challenge_length) { memcpy(s->challenge, data, length); s->challenge_length = length; }
    if(s->signature_consumed) return -1;
    if(!s->signature_length) return LIBSSH2_ERROR_EAGAIN;
    void *context = s;
    unsigned char *owned = allocate(s->signature_length, &context);
    if(!owned) return LIBSSH2_ERROR_ALLOC;
    memcpy(owned, s->signature, s->signature_length);
    *signature = owned; *signature_length = s->signature_length;
    s->signature_consumed = 1;
    memset(s->signature, 0, sizeof(s->signature));
    return 0;
}
int ftssh_authentication_step(ftssh_session *s) {
    int rc = check(s, AUTH); if(rc) return rc;
    if(s->challenge_length && !s->signature_length) return FTSSH_SIGNATURE_NEEDED;
    void *context = s;
    rc = libssh2_userauth_publickey(s->ssh, s->user, s->public_blob, s->public_length, sign_callback, &context);
    if(rc == LIBSSH2_ERROR_EAGAIN) return s->challenge_length && !s->signature_length ? FTSSH_SIGNATURE_NEEDED : FTSSH_AGAIN;
    if(rc != 0 || !libssh2_userauth_authenticated(s->ssh)) return fail(s, FTSSH_AUTH_REJECTED);
    s->phase = AUTHENTICATED; return FTSSH_OK;
}
int ftssh_copy_challenge(ftssh_session *s, uint8_t *out, size_t capacity, size_t *length) {
    int rc = check(s, AUTH); if(rc) return rc;
    if(!out || !length) return FTSSH_INVALID;
    *length = 0;
    if(!s->challenge_length || s->signature_consumed) return FTSSH_STATE;
    if(s->challenge_length > capacity) return FTSSH_LIMIT;
    memcpy(out, s->challenge, s->challenge_length); *length = s->challenge_length; return FTSSH_OK;
}
int ftssh_supply_signature(ftssh_session *s, const uint8_t *blob, size_t length) {
    int rc = check(s, AUTH); if(rc) return rc;
    if(!blob || !length || length > 512) return FTSSH_INVALID;
    if(!s->challenge_length || s->signature_length || s->signature_consumed) return FTSSH_STATE;
    const uint8_t *cursor = blob, *payload; size_t left = length, n;
    if(!expected_field(&cursor, &left, s->algorithm, strlen(s->algorithm)) ||
       !field(&cursor, &left, &payload, &n) || left || !n || n > sizeof(s->signature)) return FTSSH_CHALLENGE;
    if(strcmp(s->algorithm, "ssh-ed25519") == 0 && n != 64) return FTSSH_CHALLENGE;
    if(strcmp(s->algorithm, "ssh-ed25519") != 0) {
        const uint8_t *part = payload, *integer; size_t remaining = n, width;
        size_t maximum = s->algorithm[16] == '2' ? 32 : (s->algorithm[16] == '3' ? 48 : 66);
        for(int i = 0; i < 2; i++) {
            if(!field(&part, &remaining, &integer, &width) || !width || width > maximum + 1 ||
               (integer[0] & 128) || (integer[0] == 0 && (width == 1 || !(integer[1] & 128))) ||
               (width == maximum + 1 && integer[0] != 0)) return FTSSH_CHALLENGE;
        }
        if(remaining) return FTSSH_CHALLENGE;
    }
    memcpy(s->signature, payload, n); s->signature_length = n; return FTSSH_OK;
}
int ftssh_begin_shell(ftssh_session *s, uint32_t c, uint32_t r, uint32_t w, uint32_t h) {
    int rc = check(s, AUTHENTICATED); if(rc) return rc;
    if(!valid_geometry(c, r, w, h)) return FTSSH_INVALID;
    s->geometry[0] = c; s->geometry[1] = r; s->geometry[2] = w; s->geometry[3] = h;
    s->phase = OPENING; return FTSSH_OK;
}
int ftssh_shell_step(ftssh_session *s) {
    if(!s) return FTSSH_INVALID;
    int rc;
    if(s->phase == OPENING) {
        s->channel = libssh2_channel_open_ex(s->ssh, "session", 7, 128 * 1024, 16384, NULL, 0);
        if(!s->channel) return libssh2_session_last_errno(s->ssh) == LIBSSH2_ERROR_EAGAIN ? FTSSH_AGAIN : fail(s, FTSSH_CHANNEL);
        rc = libssh2_channel_handle_extended_data2(s->channel, LIBSSH2_CHANNEL_EXTENDED_DATA_MERGE);
        if(rc) return fail(s, FTSSH_CHANNEL);
        s->phase = PTY;
    }
    if(s->phase == PTY) {
        rc = result(s, libssh2_channel_request_pty_ex(s->channel, "xterm-256color", 14, NULL, 0,
            (int)s->geometry[0], (int)s->geometry[1], (int)s->geometry[2], (int)s->geometry[3]), FTSSH_CHANNEL);
        if(rc) return rc;
        s->phase = SHELL;
    }
    if(s->phase == SHELL) {
        rc = result(s, libssh2_channel_process_startup(s->channel, "shell", 5, NULL, 0), FTSSH_CHANNEL);
        if(rc) return rc;
        /* ftssh_keepalive probes ask for a reply; libssh2 treats 1 as 2 s. */
        libssh2_keepalive_config(s->ssh, 1, 1);
        s->phase = LIVE; return FTSSH_OK;
    }
    return check(s, LIVE);
}
int ftssh_admit_write(ftssh_session *s, const uint8_t *bytes, size_t length) {
    int rc = check(s, LIVE); if(rc) return rc;
    if(!bytes || !length || length > FTSSH_WRITE_CAPACITY) return FTSSH_INVALID;
    if(s->write_length || s->resize_pending) return FTSSH_AGAIN;
    if(length > UINT64_MAX - s->accepted) return fail(s, FTSSH_LIMIT);
    memcpy(s->write_buffer, bytes, length); s->write_length = length; s->write_offset = 0;
    s->accepted += length; return FTSSH_OK;
}
int ftssh_flush_write(ftssh_session *s) {
    int rc = check(s, LIVE); if(rc) return rc;
    if(!s->write_length) return FTSSH_OK;
    size_t left = s->write_length - s->write_offset;
    ssize_t count = libssh2_channel_write_ex(s->channel, 0, (const char *)s->write_buffer + s->write_offset, left);
    if(count == LIBSSH2_ERROR_EAGAIN || count == 0) return FTSSH_AGAIN;
    if(count < 0 || (size_t)count > left) return fail(s, FTSSH_IO);
    s->written += (size_t)count; s->write_offset += (size_t)count;
    if(s->write_offset == s->write_length) {
        memset(s->write_buffer, 0, s->write_length); s->write_offset = 0; s->write_length = 0;
        return FTSSH_OK;
    }
    return FTSSH_AGAIN;
}
int ftssh_read(ftssh_session *s, uint8_t *out, size_t capacity, size_t *length) {
    int rc = check(s, LIVE); if(rc) return rc;
    if(!out || !length || !capacity || capacity > 16384) return FTSSH_INVALID;
    *length = 0;
    ssize_t count = libssh2_channel_read_ex(s->channel, 0, (char *)out, capacity);
    if(count == LIBSSH2_ERROR_EAGAIN) return FTSSH_AGAIN;
    if(count < 0 || (size_t)count > capacity) return fail(s, FTSSH_IO);
    if(count == 0) return libssh2_channel_eof(s->channel) ? FTSSH_EOF : FTSSH_AGAIN;
    *length = (size_t)count; return FTSSH_OK;
}
int ftssh_resize(ftssh_session *s, uint32_t c, uint32_t r, uint32_t w, uint32_t h) {
    int rc = check(s, LIVE); if(rc) return rc;
    if(!valid_geometry(c, r, w, h)) return FTSSH_INVALID;
    uint32_t requested[4] = {c, r, w, h};
    if(s->write_length) return FTSSH_AGAIN;
    if(s->resize_pending && memcmp(s->resize_geometry, requested, sizeof(requested))) return FTSSH_STATE;
    memcpy(s->resize_geometry, requested, sizeof(requested)); s->resize_pending = 1;
    rc = result(s, libssh2_channel_request_pty_size_ex(s->channel, (int)c, (int)r, (int)w, (int)h), FTSSH_CHANNEL);
    if(rc == FTSSH_OK) { memcpy(s->geometry, requested, sizeof(requested)); s->resize_pending = 0; }
    return rc;
}
/* Bytes accepted by the kernel but not yet sent, or -1 when unknown. */
static int unsent_socket_bytes(int fd) {
    int queued = 0;
#if defined(SO_NWRITE)
    socklen_t length = sizeof(queued);
    if(getsockopt(fd, SOL_SOCKET, SO_NWRITE, &queued, &length)) return -1;
#elif defined(SIOCOUTQ)
    if(ioctl(fd, SIOCOUTQ, &queued)) return -1;
#else
    return -1;
#endif
    return queued;
}
int ftssh_keepalive(ftssh_session *s) {
    int rc = check(s, LIVE); if(rc) return rc;
    /* libssh2 binds a partly sent packet to the caller's buffer and refuses any
     * other packet until that exact buffer is resent; its keepalive buffer is
     * its own stack frame. Send only when nothing is pending and the whole
     * packet fits in the socket buffer, so it cannot be left half sent. */
    if(s->write_length || s->resize_pending ||
       (libssh2_session_block_directions(s->ssh) & LIBSSH2_SESSION_BLOCK_OUTBOUND)) return FTSSH_AGAIN;
    int capacity = 0, queued = unsent_socket_bytes(s->fd);
    socklen_t length = sizeof(capacity);
    if(queued < 0 || getsockopt(s->fd, SOL_SOCKET, SO_SNDBUF, &capacity, &length)) return FTSSH_UNSUPPORTED;
    if(capacity - queued < 4096) return FTSSH_AGAIN;
    int next = 0;
    if(libssh2_keepalive_send(s->ssh, &next)) return fail(s, FTSSH_IO);
    /* Still pending despite the room checked above: fail rather than block input. */
    if(libssh2_session_block_directions(s->ssh) & LIBSSH2_SESSION_BLOCK_OUTBOUND) return fail(s, FTSSH_IO);
    return FTSSH_OK;
}
int ftssh_directions(ftssh_session *s) {
    if(!s || !s->ssh || s->phase >= FAILED) return 0;
    int directions = libssh2_session_block_directions(s->ssh);
    return (directions & LIBSSH2_SESSION_BLOCK_INBOUND ? FTSSH_READ : 0) |
           (directions & LIBSSH2_SESSION_BLOCK_OUTBOUND ? FTSSH_WRITE : 0);
}
int ftssh_descriptor(ftssh_session *s) { return s ? s->fd : -1; }
int ftssh_get_stats(ftssh_session *s, ftssh_stats *out) {
    if(!s || !out) return FTSSH_INVALID;
    *out = (ftssh_stats){s->allocated, s->peak, s->write_length - s->write_offset, s->accepted, s->written,
                         s->terminal_error, s->received};
    return FTSSH_OK;
}
int ftssh_close(ftssh_session *s) {
    if(!s) return FTSSH_INVALID;
    if(s->phase == CLOSED) return FTSSH_OK;
    /* Force callback failure even if libssh2 was mid-EAGAIN. No teardown packet
     * can wait for peer cooperation and no stale fd can be used after close. */
    s->closing = 1;
    if(s->fd >= 0) { shutdown(s->fd, SHUT_RDWR); close(s->fd); s->fd = -1; }
    if(s->ssh) {
        int rc = libssh2_session_free(s->ssh);
        if(rc != 0) return fail(s, FTSSH_IO); // retain ownership for explicit retry
        s->ssh = NULL; s->channel = NULL;
    }
    memset(s->signature, 0, sizeof(s->signature));
    memset(s->write_buffer, 0, sizeof(s->write_buffer));
    s->write_length = 0; s->write_offset = 0;
    s->phase = CLOSED;
    return s->allocated == 0 ? FTSSH_OK : FTSSH_LIMIT;
}
int ftssh_destroy(ftssh_session **address) {
    if(!address || !*address) return FTSSH_OK;
    int rc = ftssh_close(*address); if(rc) return rc;
    free(*address); *address = NULL; atomic_fetch_sub(&global_sessions, 1); return FTSSH_OK;
}
