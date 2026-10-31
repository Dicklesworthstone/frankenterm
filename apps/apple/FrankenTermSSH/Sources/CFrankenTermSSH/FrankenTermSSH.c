#include "FrankenTermSSH.h"
#include <libssh2.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <poll.h>
#include <stdalign.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
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
 * Keyboard-interactive alone transfers libssh2 ownership to a bounded pthread;
 * its synchronous callback waits there for an owned response or cancellation.
 * No callback invokes Swift, throws, or retains a borrowed argument.
 * The library owns callback signatures through the same supplied allocator.
 * No global signal handler, key-file loading, shell command or trace is used.
 */
enum phase { HANDSHAKE, PEER, APPROVED, AUTH, PARTIAL_AUTH, AUTHENTICATED, FORWARD_OPENING, FORWARD_READY,
             OPENING, PTY, SHELL, LIVE, FAILED, CLOSED };
enum { EGRESS_CAPACITY = 65536, EGRESS_RESERVE = 49152 };
struct interactive_worker;
struct ftssh_session {
    LIBSSH2_SESSION *ssh;
    LIBSSH2_CHANNEL *channel;
    /* Owned exclusively after forward_adopt. Both libraries are driven by the
     * same actor; no other owner may access the adopted outer session. */
    struct ftssh_session *parent;
    /* The leaf owns the worker allocation. Every adopted ancestor points to
     * that same owner only while the worker exclusively drives the chain. */
    struct interactive_worker *interactive, *worker_owner;
    unsigned int forwarding_depth;
    /* Owned ciphertext, charged to the same local/global allocation budget.
     * A libssh2 send callback accepts a complete packet here. Calling a parent
     * library from that callback can deadlock its pending channel-data packet
     * against a read-side WINDOW_ADJUST at another nesting level. */
    uint8_t *egress;
    size_t egress_head, egress_count, egress_flight, write_receipt;
    int fd, phase, terminal_error, closing, handshake_started;
    size_t limit, allocated, peak;
    char user[257], algorithm[65], offer_algorithm[65], signature_algorithm[65];
    uint8_t public_blob[FTSSH_PUBLIC_KEY_CAPACITY], challenge[FTSSH_CHALLENGE_CAPACITY], signature[1024];
    size_t public_length, challenge_length, signature_length, rsa_signature_length;
    uint8_t password[FTSSH_PASSWORD_CAPACITY];
    size_t password_length;
    int password_authentication, password_supplied, user_certificate;
    int publickey_interactive, partial_ready;
    uint64_t authentication_deadline, publickey_attempt_baseline;
    int signature_consumed, resize_pending;
    int auth_methods_checked, sign_callback_entered, sign_error, socket_failed;
    /* The server sent a disconnect message: it answered, unlike a transport
     * that failed or closed without a word. */
    int peer_disconnected;
    uint32_t geometry[4], resize_geometry[4];
    /* A command channel (no PTY) carries binary data, so stderr is dropped. */
    char command[FTSSH_COMMAND_CAPACITY + 1];
    size_t command_length;
    uint8_t forward_request[282];
    size_t forward_length;
    uint8_t write_buffer[FTSSH_WRITE_CAPACITY];
    size_t write_length, write_offset;
    uint64_t accepted, written, received;
    /* Bytes the transport handed to its socket or outer channel. */
    uint64_t sent;
    size_t receive_budget;
    int limited_receive, output_eof;
};
struct interactive_worker {
    ftssh_session *session;
    pthread_t thread;
    pthread_mutex_t lock;
    int notification[2], control[2];
    _Atomic int cancelled;
    int done, result, callback_error, pending, supplied;
    uint32_t round, prompt_count;
    uint64_t deadline_milliseconds;
    uint8_t challenge[FTSSH_INTERACTIVE_CHALLENGE_CAPACITY];
    uint8_t responses[FTSSH_INTERACTIVE_ENCODED_RESPONSES_CAPACITY];
    size_t challenge_length, response_length;
};
typedef union {
    max_align_t alignment;
    struct { size_t size; int sensitive; } metadata;
} allocation_header;
static _Atomic size_t global_allocated;
static _Atomic unsigned int global_sessions;
static _Atomic unsigned int global_interactive_workers;
static const size_t global_limit = 16u * 1024u * 1024u;
static pthread_once_t initialization = PTHREAD_ONCE_INIT;
static int initialized;
static void initialize(void) { initialized = libssh2_init(0); }
static uint64_t monotonic_milliseconds(void);

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
    h->metadata.size = total; h->metadata.sensitive = s->phase == AUTH;
    s->allocated += total;
    if(s->allocated > s->peak) s->peak = s->allocated;
    return h + 1;
}
static void release(void *pointer, void **abstract) {
    if(!pointer) return;
    ftssh_session *s = *abstract;
    allocation_header *h = (allocation_header *)pointer - 1;
    size_t total = h->metadata.size;
    /* libssh2 frees keyboard-interactive answers and its assembled plaintext
     * authentication packet through this allocator. Preserve their sensitivity
     * past authentication and reallocation, then wipe before returning memory. */
    if(h->metadata.sensitive) {
        volatile uint8_t *bytes = pointer;
        for(size_t i = 0; i < total - sizeof(*h); i++) bytes[i] = 0;
    }
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
    allocation_header *next = (allocation_header *)replacement - 1;
    next->metadata.sensitive |= old->metadata.sensitive;
    size_t previous = old->metadata.size - sizeof(allocation_header);
    memcpy(replacement, pointer, size < previous ? size : previous);
    release(pointer, abstract); return replacement;
}
static int drain_egress(ftssh_session *s, int *progress);
static int egress_empty(ftssh_session *s) {
    for(; s; s = s->parent) if(s->egress_count) return 0;
    return 1;
}
static ssize_t socket_send(libssh2_socket_t fd, const void *data, size_t length, int flags, void **abstract) {
    (void)fd; (void)flags;
    ftssh_session *s = *abstract;
    if(s->closing || (s->worker_owner && atomic_load(&s->worker_owner->cancelled))) return -ECONNRESET;
    if(!s->egress || length > EGRESS_CAPACITY - s->egress_count) {
        s->socket_failed = 1; s->terminal_error = FTSSH_LIMIT; return -ENOBUFS;
    }
    size_t tail = (s->egress_head + s->egress_count) % EGRESS_CAPACITY;
    size_t first = length < EGRESS_CAPACITY - tail ? length : EGRESS_CAPACITY - tail;
    memcpy(s->egress + tail, data, first);
    memcpy(s->egress, (const uint8_t *)data + first, length - first);
    s->egress_count += length; s->sent += (uint64_t)length;
    return (ssize_t)length;
}
static ssize_t socket_receive(libssh2_socket_t fd, void *data, size_t length, int flags, void **abstract) {
    ftssh_session *s = *abstract;
    if(s->closing || (s->worker_owner && atomic_load(&s->worker_owner->cancelled))) return -ECONNRESET;
    int progress = 0;
    if(drain_egress(s, &progress) < 0) { s->socket_failed = 1; return -ECONNRESET; }
    if(s->limited_receive) {
        if(!s->receive_budget) return -EAGAIN;
        if(length > s->receive_budget) length = s->receive_budget;
    }
    ssize_t result;
    if(s->parent) {
        ftssh_session *p = s->parent;
        if(p->closing || p->phase != FORWARD_READY || p->socket_failed) return -ECONNRESET;
        if(p->egress_count > EGRESS_CAPACITY - EGRESS_RESERVE) return -EAGAIN;
        p->receive_budget = 32768; p->limited_receive = 1;
        result = libssh2_channel_read_ex(p->channel, 0, data, length);
        p->limited_receive = 0;
        if(result == LIBSSH2_ERROR_EAGAIN || (result == 0 && !libssh2_channel_eof(p->channel))) return -EAGAIN;
        if(result < 0 || (size_t)result > length || p->socket_failed) {
            s->socket_failed = 1; return -ECONNRESET;
        }
    } else {
        result = recv(fd, data, length, flags);
    }
    if((result < 0 && errno != EAGAIN && errno != EWOULDBLOCK) || (!result && length)) s->socket_failed = 1;
    if(result > 0) {
        if(s->limited_receive) s->receive_budget -= (size_t)result;
        if((uint64_t)result <= UINT64_MAX - s->received) s->received += (uint64_t)result;
    }
    return result < 0 ? -errno : result;
}

/* One finite turn per layer. Parent writes keep the exact owned span across
 * EAGAIN; appending later ciphertext cannot change that outstanding span.
 * A parent library can only invoke callbacks on an earlier ancestor, so this
 * bounded recursion never re-enters the library that is currently executing. */
static int drain_egress(ftssh_session *s, int *progress) {
    if(!s || s->closing || s->socket_failed) return FTSSH_IO;
    if(s->parent && drain_egress(s->parent, progress) < 0) return FTSSH_IO;
    for(unsigned int turns = 0; s->egress_count && turns < 4; turns++) {
        size_t contiguous = EGRESS_CAPACITY - s->egress_head;
        if(contiguous > s->egress_count) contiguous = s->egress_count;
        ssize_t count;
        if(s->parent) {
            ftssh_session *p = s->parent;
            if(p->phase != FORWARD_READY || p->egress_count > EGRESS_CAPACITY - EGRESS_RESERVE) break;
            if(!s->egress_flight) s->egress_flight = contiguous < 16384 ? contiguous : 16384;
            p->receive_budget = 32768; p->limited_receive = 1;
            count = libssh2_channel_write_ex(p->channel, 0, (const char *)s->egress + s->egress_head, s->egress_flight);
            p->limited_receive = 0;
            if(count == LIBSSH2_ERROR_EAGAIN || count == 0) {
                if(drain_egress(p, progress) < 0) return FTSSH_IO;
                break;
            }
            if(count < 0 || (size_t)count > s->egress_flight || p->socket_failed) return FTSSH_IO;
            s->egress_flight = 0;
        } else {
            int flags = 0;
#ifdef MSG_NOSIGNAL
            flags = MSG_NOSIGNAL;
#endif
            count = send(s->fd, s->egress + s->egress_head, contiguous, flags);
            if(count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) break;
            if(count <= 0 || (size_t)count > contiguous) { s->socket_failed = 1; return FTSSH_IO; }
        }
        s->egress_head = (s->egress_head + (size_t)count) % EGRESS_CAPACITY;
        s->egress_count -= (size_t)count; *progress = 1;
        if(s->parent && drain_egress(s->parent, progress) < 0) return FTSSH_IO;
    }
    return s->egress_count ? FTSSH_AGAIN : FTSSH_OK;
}
static void socket_disconnected(LIBSSH2_SESSION *ssh, int reason, const char *message, int message_length,
                                const char *language, int language_length, void **abstract) {
    (void)ssh; (void)reason; (void)message; (void)message_length; (void)language; (void)language_length;
    ftssh_session *s = *abstract;
    s->socket_failed = 1; s->peer_disconnected = 1;
}
static void clear_password(ftssh_session *s) {
    /* A retained handle must not retain the completed prompt's secret. The
     * volatile writes also survive optimization of a subsequent destroy. */
    volatile uint8_t *bytes = s->password;
    for(size_t i = 0; i < sizeof(s->password); i++) bytes[i] = 0;
    s->password_length = 0;
}
static int fail(ftssh_session *s, int code) {
    clear_password(s);
    s->partial_ready = 0;
    s->phase = FAILED; s->terminal_error = code; return code;
}
static int publickey_deadline(ftssh_session *s) {
    if(!s->publickey_interactive) return FTSSH_OK;
    uint64_t now = monotonic_milliseconds();
    if(!now) return fail(s, FTSSH_IO);
    return now >= s->authentication_deadline ? fail(s, FTSSH_AUTH_TIMEOUT) : FTSSH_OK;
}
static int check(ftssh_session *s, int expected) {
    if(!s) return FTSSH_INVALID;
    if(s->worker_owner) return FTSSH_STATE;
    if(s->phase == CLOSED) return FTSSH_CLOSED;
    if(s->phase == FAILED) return s->terminal_error;
    return s->phase == expected ? FTSSH_OK : FTSSH_STATE;
}
static int prepare_egress(ftssh_session *s) {
    if(s->phase == CLOSED) return FTSSH_CLOSED;
    if(s->phase == FAILED) return s->terminal_error;
    int progress = 0;
    if(drain_egress(s, &progress) < 0) return fail(s, s->terminal_error == FTSSH_LIMIT ? FTSSH_LIMIT : FTSSH_IO);
    for(ftssh_session *p = s; p; p = p->parent)
        if(p->egress_count > EGRESS_CAPACITY - EGRESS_RESERVE) return FTSSH_AGAIN;
    s->receive_budget = 32768; s->limited_receive = 1;
    return FTSSH_OK;
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
static int positive_mpint(const uint8_t **bytes, size_t *left, const uint8_t **value,
                          size_t *length, size_t maximum) {
    if(!field(bytes, left, value, length) || !*length || *length > maximum + 1 || (**value & 128)) return 0;
    if(**value == 0) {
        if(*length == 1 || !((*value)[1] & 128)) return 0;
        (*value)++; (*length)--;
    }
    return *length <= maximum;
}
static uint64_t certificate_u64(const uint8_t *bytes) {
    uint64_t value = 0;
    for(size_t i = 0; i < 8; i++) value = (value << 8) | bytes[i];
    return value;
}
static int certificate_time_valid(const uint64_t validity[2], time_t now) {
    if(now == (time_t)-1 || now < 0) return 0;
    uint64_t seconds = (uint64_t)now;
    return validity[0] <= seconds && seconds < validity[1];
}
static int certificate_extensions(const uint8_t *cursor, size_t left) {
    const uint8_t *previous = NULL; size_t previous_length = 0, count = 0;
    while(left) {
        const uint8_t *name, *data; size_t length, data_length;
        if(++count > 256 || !field(&cursor, &left, &name, &length) || !length || length > 256 ||
           !field(&cursor, &left, &data, &data_length)) return 0;
        for(size_t i = 0; i < length; i++) if(name[i] < 33 || name[i] > 126) return 0;
        if(previous) {
            size_t common = previous_length < length ? previous_length : length;
            int order = memcmp(previous, name, common);
            if(order > 0 || (order == 0 && previous_length >= length)) return 0;
        }
        static const char *const flags[] = {"permit-X11-forwarding", "permit-agent-forwarding",
            "permit-port-forwarding", "permit-pty", "permit-user-rc"};
        for(size_t i = 0; i < sizeof(flags) / sizeof(flags[0]); i++)
            if(length == strlen(flags[i]) && !memcmp(name, flags[i], length) && data_length) return 0;
        previous = name; previous_length = length;
    }
    return 1;
}
static int signature_payload(const char *algorithm, size_t rsa_length, const uint8_t *payload, size_t length) {
    if(!length || length > 1024) return 0;
    if(rsa_length) return length == rsa_length;
    if(!strcmp(algorithm, "ssh-ed25519")) return length == 64;
    const uint8_t *cursor = payload, *integer; size_t left = length, width;
    size_t maximum = algorithm[16] == '2' ? 32 : (algorithm[16] == '3' ? 48 : 66);
    for(int i = 0; i < 2; i++)
        if(!positive_mpint(&cursor, &left, &integer, &width, maximum)) return 0;
    return left == 0;
}
/* Allocation-free structural admission. The workspace parser independently
 * verifies the CA signature, subject binding, time and route permissions.
 * Recursion is limited to one raw CA key; chained certificates are refused. */
static int public_key_profile(const uint8_t *blob, size_t length, int allow_certificate,
                              char algorithm[65], char offer_algorithm[65],
                              size_t *rsa_signature_length, int *is_certificate, uint64_t validity[2]) {
    if(!blob || !length || length > FTSSH_PUBLIC_KEY_CAPACITY) return FTSSH_INVALID;
    const uint8_t *cursor = blob, *name; size_t left = length, n;
    if(!field(&cursor, &left, &name, &n) || !n || n >= 65) return FTSSH_INVALID;
    memcpy(offer_algorithm, name, n); offer_algorithm[n] = 0;
    static const char suffix[] = "-cert-v01@openssh.com";
    *is_certificate = n > sizeof(suffix) - 1 &&
        equal(name + n - (sizeof(suffix) - 1), (const uint8_t *)suffix, sizeof(suffix) - 1);
    if(*is_certificate) {
        if(!allow_certificate) return FTSSH_UNSUPPORTED;
        n -= sizeof(suffix) - 1;
        const uint8_t *nonce; size_t nonce_length;
        if(!field(&cursor, &left, &nonce, &nonce_length)) return FTSSH_INVALID;
    } else if(length > FTSSH_RAW_PUBLIC_KEY_CAPACITY) return FTSSH_INVALID;
    if(!((n == 7 && equal(name, (const uint8_t *)"ssh-rsa", n)) ||
         (n == 11 && equal(name, (const uint8_t *)"ssh-ed25519", n)) ||
         (n == 19 && (equal(name, (const uint8_t *)"ecdsa-sha2-nistp256", n) ||
                      equal(name, (const uint8_t *)"ecdsa-sha2-nistp384", n) ||
                      equal(name, (const uint8_t *)"ecdsa-sha2-nistp521", n))))) return FTSSH_UNSUPPORTED;
    memcpy(algorithm, name, n); algorithm[n] = 0;
    *rsa_signature_length = 0; validity[0] = validity[1] = 0;
    const uint8_t *point; size_t point_length;
    if(n == 7) {
        const uint8_t *exponent, *modulus; size_t exponent_length, modulus_length;
        if(!positive_mpint(&cursor, &left, &exponent, &exponent_length, 8) ||
           !positive_mpint(&cursor, &left, &modulus, &modulus_length, 1024) ||
           !(exponent[exponent_length - 1] & 1) || !(modulus[modulus_length - 1] & 1) ||
           (exponent_length == 1 && exponent[0] < 3)) return FTSSH_INVALID;
        size_t bits = (modulus_length - 1) * 8;
        for(unsigned int high = modulus[0]; high; high >>= 1) bits++;
        if(bits < 2048 || bits > 8192) return FTSSH_UNSUPPORTED;
        *rsa_signature_length = modulus_length;
    } else if(n == 11) {
        if(!field(&cursor, &left, &point, &point_length) || point_length != 32) return FTSSH_INVALID;
    } else {
        if(!expected_field(&cursor, &left, name + 11, 8) ||
           !field(&cursor, &left, &point, &point_length)) return FTSSH_INVALID;
        size_t width = name[16] == '2' ? 65 : (name[16] == '3' ? 97 : 133);
        if(point_length != width || point[0] != 4) return FTSSH_INVALID;
    }
    if(!*is_certificate) return left ? FTSSH_INVALID : FTSSH_OK;
    /* serial followed by SSH_CERT_TYPE_USER; neither serial nor key ID is
     * substituted for the private subject identity. */
    if(left < 12 || cursor[8] || cursor[9] || cursor[10] || cursor[11] != 1) return FTSSH_INVALID;
    cursor += 12; left -= 12;
    const uint8_t *value; size_t value_length;
    if(!field(&cursor, &left, &value, &value_length) ||
       !field(&cursor, &left, &value, &value_length)) return FTSSH_INVALID;
    const uint8_t *principals = value; size_t principals_left = value_length, principal_count = 0;
    while(principals_left) {
        if(++principal_count > 256 || !field(&principals, &principals_left, &value, &value_length) ||
           !value_length || value_length > 256) return FTSSH_INVALID;
        for(size_t i = 0; i < value_length; i++) if(value[i] < 32 || value[i] == 127) return FTSSH_INVALID;
    }
    if(left < 16 || certificate_u64(cursor) >= certificate_u64(cursor + 8)) return FTSSH_INVALID;
    validity[0] = certificate_u64(cursor); validity[1] = certificate_u64(cursor + 8);
    cursor += 16; left -= 16;
    if(!field(&cursor, &left, &value, &value_length)) return FTSSH_INVALID;
    if(value_length) return FTSSH_UNSUPPORTED; /* no qualified critical options */
    if(!field(&cursor, &left, &value, &value_length) || !certificate_extensions(value, value_length) ||
       !field(&cursor, &left, &value, &value_length) || value_length) return FTSSH_INVALID;
    if(!field(&cursor, &left, &value, &value_length)) return FTSSH_INVALID;
    char authority_algorithm[65], authority_offer[65]; size_t authority_rsa; int authority_certificate;
    uint64_t authority_validity[2];
    int rc = public_key_profile(value, value_length, 0, authority_algorithm, authority_offer,
                                &authority_rsa, &authority_certificate, authority_validity);
    if(rc) return rc;
    if(!field(&cursor, &left, &value, &value_length) || left || value_length > FTSSH_SIGNATURE_CAPACITY) return FTSSH_INVALID;
    const uint8_t *signature_name, *signature; size_t signature_name_length, signature_length;
    const uint8_t *signature_cursor = value; size_t signature_left = value_length;
    if(!field(&signature_cursor, &signature_left, &signature_name, &signature_name_length) ||
       !field(&signature_cursor, &signature_left, &signature, &signature_length) || signature_left || !signature_length) return FTSSH_INVALID;
    if(authority_rsa) {
        if(signature_name_length != 12 ||
           !(equal(signature_name, (const uint8_t *)"rsa-sha2-256", 12) ||
             equal(signature_name, (const uint8_t *)"rsa-sha2-512", 12))) return FTSSH_UNSUPPORTED;
        /* SSH RSA certificate issuers may omit leading zero signature octets.
         * Swift left-pads before verifying this exact certificate. Our own
         * USERAUTH signatures still require the complete modulus width. */
        if(signature_length > authority_rsa) return FTSSH_INVALID;
    } else if(signature_name_length != strlen(authority_algorithm) ||
              !equal(signature_name, (const uint8_t *)authority_algorithm, signature_name_length) ||
              !signature_payload(authority_algorithm, 0, signature, signature_length)) return FTSSH_INVALID;
    return FTSSH_OK;
}
static int valid_geometry(uint32_t c, uint32_t r, uint32_t w, uint32_t h) {
    return c && r && c <= 512 && r <= 512 && c * r <= 65536 && w <= 65535 && h <= 65535;
}
static int result(ftssh_session *s, int rc, int failure) {
    if(rc == LIBSSH2_ERROR_EAGAIN) return FTSSH_AGAIN;
    return rc == 0 ? FTSSH_OK : fail(s, failure);
}

/* Certificate preference applies only to routes which import CA trust. Keep
 * the raw default for existing callers and first-use fingerprint probes. */
#define FTSSH_RAW_HOST_ALGORITHMS "ssh-ed25519,ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,ecdsa-sha2-nistp521,rsa-sha2-512,rsa-sha2-256"
#define FTSSH_CERT_HOST_ALGORITHMS "ssh-ed25519-cert-v01@openssh.com,ecdsa-sha2-nistp256-cert-v01@openssh.com,ecdsa-sha2-nistp384-cert-v01@openssh.com,ecdsa-sha2-nistp521-cert-v01@openssh.com,rsa-sha2-512-cert-v01@openssh.com,rsa-sha2-256-cert-v01@openssh.com"

int ftssh_supports_host_certificates(void) {
#if defined(LIBSSH2_FRANKENTERM_HOST_CERTIFICATE_VERSION) && LIBSSH2_FRANKENTERM_HOST_CERTIFICATE_VERSION == 1
    return libssh2_frankenterm_host_certificate_version() == 1;
#else
    return 0;
#endif
}
int ftssh_set_host_key_policy(ftssh_session *s, int policy) {
    int rc = check(s, HANDSHAKE); if(rc) return rc;
    if(s->handshake_started) return FTSSH_STATE;
    const char *allowed;
    switch(policy) {
    case FTSSH_HOST_RAW: allowed = FTSSH_RAW_HOST_ALGORITHMS; break;
    case FTSSH_HOST_CERTIFICATE: allowed = FTSSH_CERT_HOST_ALGORITHMS; break;
    case FTSSH_HOST_MIXED: allowed = FTSSH_CERT_HOST_ALGORITHMS "," FTSSH_RAW_HOST_ALGORITHMS; break;
    default: return FTSSH_INVALID;
    }
    if(policy != FTSSH_HOST_RAW && !ftssh_supports_host_certificates()) return FTSSH_UNSUPPORTED;
    return libssh2_session_method_pref(s->ssh, LIBSSH2_METHOD_HOSTKEY, allowed) ? FTSSH_UNSUPPORTED : FTSSH_OK;
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
    void *context = s;
    s->egress = allocate(EGRESS_CAPACITY, &context);
    if(!s->egress) {
        libssh2_session_free(s->ssh); free(s);
        atomic_fetch_sub(&global_sessions, 1); return FTSSH_LIMIT;
    }
    libssh2_session_set_blocking(s->ssh, 0);
    libssh2_session_callback_set2(s->ssh, LIBSSH2_CALLBACK_SEND, (libssh2_cb_generic *)socket_send);
    libssh2_session_callback_set2(s->ssh, LIBSSH2_CALLBACK_RECV, (libssh2_cb_generic *)socket_receive);
    libssh2_session_callback_set2(s->ssh, LIBSSH2_CALLBACK_DISCONNECT, (libssh2_cb_generic *)socket_disconnected);
    const struct { int method; const char *allowed; } policies[] = {
        {LIBSSH2_METHOD_KEX, "curve25519-sha256,curve25519-sha256@libssh.org,ecdh-sha2-nistp256,ecdh-sha2-nistp384,ecdh-sha2-nistp521"},
        {LIBSSH2_METHOD_HOSTKEY, FTSSH_RAW_HOST_ALGORITHMS},
        {LIBSSH2_METHOD_SIGN_ALGO, "rsa-sha2-512,rsa-sha2-256"},
        {LIBSSH2_METHOD_CRYPT_CS, "aes256-ctr,aes128-ctr"}, {LIBSSH2_METHOD_CRYPT_SC, "aes256-ctr,aes128-ctr"},
        {LIBSSH2_METHOD_MAC_CS, "hmac-sha2-256-etm@openssh.com,hmac-sha2-512-etm@openssh.com"},
        {LIBSSH2_METHOD_MAC_SC, "hmac-sha2-256-etm@openssh.com,hmac-sha2-512-etm@openssh.com"},
        {LIBSSH2_METHOD_COMP_CS, "none"}, {LIBSSH2_METHOD_COMP_SC, "none"}
    };
    for(size_t i = 0; i < sizeof(policies)/sizeof(policies[0]); i++) {
        if(libssh2_session_method_pref(s->ssh, policies[i].method, policies[i].allowed)) {
            /* Creation has not taken ownership of the caller's fd yet. */
            s->closing = 1; libssh2_session_free(s->ssh); release(s->egress, &context); free(s);
            atomic_fetch_sub(&global_sessions, 1); return FTSSH_UNSUPPORTED;
        }
    }
    *out = s; return FTSSH_OK;
}
int ftssh_handshake(ftssh_session *s) {
    int rc = check(s, HANDSHAKE); if(rc) return rc;
    s->handshake_started = 1;
    rc = prepare_egress(s); if(rc) return rc;
    rc = libssh2_session_handshake(s->ssh, s->fd);
    /* libssh2 reports a transport lost mid-exchange as a key exchange failure;
     * our callbacks saw the loss, and it is not the server's answer. */
    if(rc && rc != LIBSSH2_ERROR_EAGAIN && s->socket_failed && !s->peer_disconnected) return fail(s, FTSSH_IO);
    rc = result(s, rc, FTSSH_NEGOTIATION);
    if(rc == FTSSH_OK) s->phase = PEER;
    return rc;
}
int ftssh_copy_host_key(ftssh_session *s, uint8_t *out, size_t capacity, size_t *length) {
    if(!s || !out || !length) return FTSSH_INVALID;
    *length = 0;
    if(s->worker_owner) return FTSSH_STATE;
    if(s->phase < PEER || s->phase >= FAILED) return FTSSH_STATE;
    size_t n = 0; int type = 0;
    const char *key = libssh2_session_hostkey(s->ssh, &n, &type);
    if(!key || !n || n > 65536) return fail(s, FTSSH_NEGOTIATION);
    if(n > capacity) return FTSSH_LIMIT;
    memcpy(out, key, n); *length = n; return FTSSH_OK;
}
int ftssh_copy_method(ftssh_session *s, int method, char *out, size_t capacity) {
    if(!s || !out || !capacity || method < 0 || method > LIBSSH2_METHOD_COMP_SC) return FTSSH_INVALID;
    if(s->worker_owner) return FTSSH_STATE;
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
    if(!user || !user_length || user_length > 256 || !blob || !length || length > FTSSH_PUBLIC_KEY_CAPACITY) return FTSSH_INVALID;
    for(size_t i = 0; i < user_length; i++) if((unsigned char)user[i] < 32 || user[i] == 127) return FTSSH_INVALID;
    char algorithm[65], offer_algorithm[65]; size_t rsa_signature_length; int certificate;
    uint64_t validity[2];
    rc = public_key_profile(blob, length, 1, algorithm, offer_algorithm, &rsa_signature_length, &certificate, validity);
    if(rc) return rc;
    /* The Swift authority verifies issuer/subject/permissions and repeats time
     * checks at signing and MFA completion. Recheck the actual offer boundary
     * too: an await after identity selection may have crossed its expiry.
     * Established channels never call this admission function. */
    if(certificate && !certificate_time_valid(validity, time(NULL))) return FTSSH_AUTH_FAILED;
    memcpy(s->algorithm, algorithm, strlen(algorithm) + 1);
    memcpy(s->offer_algorithm, offer_algorithm, strlen(offer_algorithm) + 1);
    memcpy(s->user, user, user_length); s->user[user_length] = 0;
    memcpy(s->public_blob, blob, length); s->public_length = length;
    s->rsa_signature_length = rsa_signature_length;
    s->user_certificate = certificate;
#if defined(LIBSSH2_FRANKENTERM_AUTH_EVIDENCE_VERSION) && LIBSSH2_FRANKENTERM_AUTH_EVIDENCE_VERSION == 1
    s->publickey_attempt_baseline = libssh2_frankenterm_publickey_attempt(s->ssh);
#endif
    s->phase = AUTH; return FTSSH_OK;
}
int ftssh_supports_publickey_interactive(void) {
#if defined(LIBSSH2_FRANKENTERM_AUTH_EVIDENCE_VERSION) && LIBSSH2_FRANKENTERM_AUTH_EVIDENCE_VERSION == 1
    return libssh2_frankenterm_auth_evidence_version() == 1;
#else
    return 0;
#endif
}
int ftssh_begin_publickey_interactive_authentication(ftssh_session *s,
    const char *user, size_t user_length, const uint8_t *blob, size_t length,
    uint32_t timeout_milliseconds) {
    int rc = check(s, APPROVED); if(rc) return rc;
    if(!timeout_milliseconds || timeout_milliseconds > 120000) return FTSSH_INVALID;
    if(!ftssh_supports_publickey_interactive()) return FTSSH_UNSUPPORTED;
    uint64_t now = monotonic_milliseconds(); if(!now) return FTSSH_IO;
    rc = ftssh_begin_authentication(s, user, user_length, blob, length); if(rc) return rc;
    s->publickey_interactive = 1;
    s->authentication_deadline = now + timeout_milliseconds;
    return FTSSH_OK;
}
int ftssh_begin_password_authentication(ftssh_session *s, const char *user, size_t user_length) {
    int rc = check(s, APPROVED); if(rc) return rc;
    if(!user || !user_length || user_length > 256) return FTSSH_INVALID;
    for(size_t i = 0; i < user_length; i++) if((unsigned char)user[i] < 32 || user[i] == 127) return FTSSH_INVALID;
    memcpy(s->user, user, user_length); s->user[user_length] = 0;
    s->password_authentication = 1; s->phase = AUTH;
    return FTSSH_OK;
}
int ftssh_supply_password(ftssh_session *s, const uint8_t *password, size_t length) {
    int rc = check(s, AUTH); if(rc) return rc;
    if(!s->password_authentication || !s->auth_methods_checked || s->password_supplied) return FTSSH_STATE;
    if(!password || !length || length > FTSSH_PASSWORD_CAPACITY) return FTSSH_INVALID;
    memcpy(s->password, password, length); s->password_length = length; s->password_supplied = 1;
    return FTSSH_OK;
}
static int reject_challenge(ftssh_session *s) {
    s->sign_error = FTSSH_CHALLENGE;
    return LIBSSH2_ERROR_INVAL;
}
static int sign_callback(LIBSSH2_SESSION *ssh, unsigned char **signature, size_t *signature_length,
                         const unsigned char *data, size_t length, void **abstract) {
    (void)ssh;
    ftssh_session *s = *abstract;
    s->sign_callback_entered = 1;
    *signature = NULL; *signature_length = 0;
    int timeout = publickey_deadline(s);
    if(timeout) { s->sign_error = timeout; return LIBSSH2_ERROR_INVAL; }
    const uint8_t *cursor = data, *sid; size_t left = length, sid_length;
    if(length > sizeof(s->challenge) || !field(&cursor, &left, &sid, &sid_length) ||
       !(sid_length == 32 || sid_length == 48 || sid_length == 64) || !left || *cursor++ != 50) return reject_challenge(s);
    left--;
    if(!expected_field(&cursor, &left, s->user, strlen(s->user)) ||
       !expected_field(&cursor, &left, "ssh-connection", 14) ||
       !expected_field(&cursor, &left, "publickey", 9) || !left || *cursor++ != 1) return reject_challenge(s);
    left--;
    const uint8_t *selected_algorithm; size_t selected_length;
    if(!field(&cursor, &left, &selected_algorithm, &selected_length) ||
       !selected_length || selected_length >= sizeof(s->signature_algorithm) ||
       !expected_field(&cursor, &left, s->public_blob, s->public_length) || left) return reject_challenge(s);
    const uint8_t *signature_algorithm = selected_algorithm;
    size_t signature_algorithm_length = selected_length;
    if(s->user_certificate) {
        static const char suffix[] = "-cert-v01@openssh.com";
        if(selected_length <= sizeof(suffix) - 1 ||
           !equal(selected_algorithm + selected_length - (sizeof(suffix) - 1),
                  (const uint8_t *)suffix, sizeof(suffix) - 1)) return reject_challenge(s);
        signature_algorithm_length -= sizeof(suffix) - 1;
    }
    if(s->rsa_signature_length) {
        if(signature_algorithm_length != 12 ||
           !(equal(signature_algorithm, (const uint8_t *)"rsa-sha2-256", 12) ||
             equal(signature_algorithm, (const uint8_t *)"rsa-sha2-512", 12))) {
            /* Missing server-sig-algs leaves libssh2's unsigned offer at
             * ssh-rsa despite SIGN_ALGO preferences. Never sign it. In
             * particular do not return ALGO_UNSUPPORTED: libssh2 interprets
             * that callback result as permission to retry its default method. */
            s->sign_error = FTSSH_UNSUPPORTED; return LIBSSH2_ERROR_INVAL;
        }
    } else if(signature_algorithm_length != strlen(s->algorithm) ||
              !equal(signature_algorithm, (const uint8_t *)s->algorithm, signature_algorithm_length)) return reject_challenge(s);
    if(s->challenge_length && (s->challenge_length != length || !equal(s->challenge, data, length))) return reject_challenge(s);
    if(!s->challenge_length) {
        memcpy(s->challenge, data, length); s->challenge_length = length;
        memcpy(s->signature_algorithm, signature_algorithm, signature_algorithm_length);
        s->signature_algorithm[signature_algorithm_length] = 0;
    }
    if(s->signature_consumed) return reject_challenge(s);
    if(!s->signature_length) return LIBSSH2_ERROR_EAGAIN;
    void *context = s;
    unsigned char *owned = allocate(s->signature_length, &context);
    if(!owned) { s->sign_error = FTSSH_LIMIT; return LIBSSH2_ERROR_ALLOC; }
    memcpy(owned, s->signature, s->signature_length);
    *signature = owned; *signature_length = s->signature_length;
    s->signature_consumed = 1;
    memset(s->signature, 0, sizeof(s->signature));
    return 0;
}

/* The public library API returns a terminated name-list. Bound the complete
 * list and validate every token before accepting the exact selected method. */
static int authentication_method(const char *methods, const char *wanted) {
    if(!methods) return FTSSH_AUTH_FAILED;
    size_t length = strnlen(methods, 1025), start = 0;
    if(length > 1024) return FTSSH_LIMIT;
    if(!length) return FTSSH_UNSUPPORTED;
    int found = 0;
    for(size_t i = 0; i <= length; i++) {
        if(i == length || methods[i] == ',') {
            if(i == start) return FTSSH_AUTH_FAILED;
            size_t wanted_length = strlen(wanted);
            if(i - start == wanted_length && memcmp(methods + start, wanted, wanted_length) == 0) found = 1;
            start = i + 1;
        } else if((unsigned char)methods[i] < 33 || (unsigned char)methods[i] > 126) {
            return FTSSH_AUTH_FAILED;
        }
    }
    return found ? FTSSH_OK : FTSSH_UNSUPPORTED;
}

static int authentication_result(ftssh_session *s, int rc, int authenticated, int offered) {
    /* libssh2 overwrites receive and callback errors with PUBLICKEY_UNVERIFIED.
     * Preserve evidence from our own callbacks instead of guessing from it. */
    if(s->socket_failed) return fail(s, FTSSH_IO);
    if(s->sign_error) return fail(s, s->sign_error);
    int timeout = publickey_deadline(s); if(timeout) return timeout;
    if(authenticated && (!offered || !s->sign_callback_entered || !s->signature_consumed))
        return fail(s, FTSSH_UNSUPPORTED);
    if(rc == LIBSSH2_ERROR_EAGAIN)
        return s->challenge_length && !s->signature_length ? FTSSH_SIGNATURE_NEEDED : FTSSH_AGAIN;
    if(!rc && authenticated) {
        s->phase = AUTHENTICATED; return FTSSH_OK;
    }
#if defined(LIBSSH2_FRANKENTERM_AUTH_EVIDENCE_VERSION) && LIBSSH2_FRANKENTERM_AUTH_EVIDENCE_VERSION == 1
    if(s->publickey_interactive && offered && s->auth_methods_checked &&
       rc == LIBSSH2_ERROR_PUBLICKEY_UNVERIFIED && !authenticated &&
       s->sign_callback_entered && s->signature_consumed && s->challenge_length) {
        char methods[LIBSSH2_FRANKENTERM_AUTH_METHODS_MAX + 1];
        size_t length = 0; int partial = 0; libssh2_uint64_t attempt = 0;
        int evidence = libssh2_frankenterm_publickey_failure(s->ssh, &attempt, &partial,
                                                           methods, sizeof(methods), &length);
        if(evidence == 1 && partial && attempt > s->publickey_attempt_baseline &&
           attempt == libssh2_frankenterm_publickey_attempt(s->ssh) && length < sizeof(methods)) {
            int method = authentication_method(methods, "keyboard-interactive");
            if(method) return fail(s, method);
            s->partial_ready = 1; s->phase = PARTIAL_AUTH;
            return FTSSH_AUTH_PARTIAL_REQUIRED;
        }
        return fail(s, FTSSH_AUTH_FAILED);
    }
    /* Stock AUTHENTICATION_FAILED also covers malformed and partial=true
     * unsigned replies. Only the versioned, consuming proof can justify
     * trying another identity; an unpatched backend fails terminally. */
    if(ftssh_supports_publickey_interactive() && offered && s->auth_methods_checked &&
       rc == LIBSSH2_ERROR_AUTHENTICATION_FAILED && !s->rsa_signature_length &&
       !s->sign_callback_entered && !s->challenge_length && !s->signature_consumed) {
        libssh2_uint64_t attempt = libssh2_frankenterm_publickey_unsigned_refusal(s->ssh);
        if(attempt > s->publickey_attempt_baseline &&
           attempt == libssh2_frankenterm_publickey_attempt(s->ssh)) return fail(s, FTSSH_AUTH_REJECTED);
    }
#endif
    /* RSA's public API does not reveal which unsigned algorithm was offered.
     * A refusal can therefore mean missing SHA-2 support, not a refused key;
     * it cannot authorize automatic iteration over other private identities. */
    switch(rc) {
    case LIBSSH2_ERROR_ALLOC: return fail(s, FTSSH_LIMIT);
    case LIBSSH2_ERROR_SOCKET_NONE: case LIBSSH2_ERROR_SOCKET_SEND:
    case LIBSSH2_ERROR_SOCKET_DISCONNECT: case LIBSSH2_ERROR_SOCKET_RECV:
    case LIBSSH2_ERROR_BAD_SOCKET: return fail(s, FTSSH_IO);
    case LIBSSH2_ERROR_METHOD_NONE: case LIBSSH2_ERROR_METHOD_NOT_SUPPORTED:
    case LIBSSH2_ERROR_ALGO_UNSUPPORTED: return fail(s, FTSSH_UNSUPPORTED);
    default: return fail(s, FTSSH_AUTH_FAILED);
    }
}
static int password_authentication_result(ftssh_session *s, int rc, int authenticated, int offered) {
    if(s->socket_failed) return fail(s, FTSSH_IO);
    if(authenticated && (!offered || !s->password_supplied)) return fail(s, FTSSH_UNSUPPORTED);
    if(rc == LIBSSH2_ERROR_EAGAIN) return FTSSH_AGAIN;
    if(!rc && authenticated) {
        clear_password(s); s->phase = AUTHENTICATED; return FTSSH_OK;
    }
    switch(rc) {
    case LIBSSH2_ERROR_ALLOC: return fail(s, FTSSH_LIMIT);
    case LIBSSH2_ERROR_SOCKET_NONE: case LIBSSH2_ERROR_SOCKET_SEND:
    case LIBSSH2_ERROR_SOCKET_DISCONNECT: case LIBSSH2_ERROR_SOCKET_RECV:
    case LIBSSH2_ERROR_BAD_SOCKET: return fail(s, FTSSH_IO);
    case LIBSSH2_ERROR_METHOD_NONE: case LIBSSH2_ERROR_METHOD_NOT_SUPPORTED:
    case LIBSSH2_ERROR_ALGO_UNSUPPORTED: return fail(s, FTSSH_UNSUPPORTED);
    default: return fail(s, FTSSH_PASSWORD_FAILED);
    }
}

static void store_u32(uint8_t *out, uint32_t value);
static int transport_directions(ftssh_session *s);

static uint64_t monotonic_milliseconds(void) {
    struct timespec now;
    if(clock_gettime(CLOCK_MONOTONIC, &now)) return 0;
    return (uint64_t)now.tv_sec * 1000 + (uint64_t)now.tv_nsec / 1000000;
}
static void signal_pipe(int fd) {
    uint8_t byte = 1;
    ssize_t written;
    do { written = write(fd, &byte, 1); } while(written < 0 && errno == EINTR);
    /* EAGAIN already means the notification remains readable. Both ends stay
     * open until the worker is joined, so cancellation cannot cause SIGPIPE. */
}
static void drain_pipe(int fd) {
    uint8_t bytes[64];
    for(unsigned int turn = 0; turn < 4; turn++) {
        ssize_t n = read(fd, bytes, sizeof(bytes));
        if(n <= 0 && errno != EINTR) break;
    }
}
static int owned_pipe(int ends[2]) {
    if(pipe(ends)) return 0;
    for(unsigned int i = 0; i < 2; i++) {
        int flags = fcntl(ends[i], F_GETFL), descriptor_flags = fcntl(ends[i], F_GETFD);
        if(flags < 0 || descriptor_flags < 0 || fcntl(ends[i], F_SETFL, flags | O_NONBLOCK) < 0 ||
           fcntl(ends[i], F_SETFD, descriptor_flags | FD_CLOEXEC) < 0) {
            close(ends[0]); close(ends[1]); ends[0] = ends[1] = -1; return 0;
        }
    }
    return 1;
}
static void clear_interactive_responses(struct interactive_worker *worker) {
    volatile uint8_t *bytes = worker->responses;
    for(size_t i = 0; i < sizeof(worker->responses); i++) bytes[i] = 0;
    worker->response_length = 0;
}
static int interactive_stop(struct interactive_worker *worker) {
    if(atomic_load(&worker->cancelled)) return FTSSH_CLOSED;
    uint64_t now = monotonic_milliseconds();
    if(!now) return FTSSH_IO;
    return now >= worker->deadline_milliseconds ? FTSSH_AUTH_TIMEOUT : FTSSH_OK;
}
/* The only blocking waits occur on the admitted pthread. A separate control
 * pipe wakes both socket polling and callback waits; the UI never needs a lock
 * held across those waits. Every iteration rechecks the monotonic deadline. */
static int interactive_wait(struct interactive_worker *worker, int directions) {
    int stopped = interactive_stop(worker); if(stopped) return stopped;
    uint64_t now = monotonic_milliseconds();
    if(!now) return FTSSH_IO;
    if(now >= worker->deadline_milliseconds) return FTSSH_AUTH_TIMEOUT;
    uint64_t remaining = worker->deadline_milliseconds - now;
    int milliseconds = remaining > 100 ? 100 : (int)remaining;
    struct pollfd descriptors[2] = {
        { worker->control[0], POLLIN, 0 },
        { directions ? worker->session->fd : -1,
          (short)((directions & FTSSH_READ ? POLLIN : 0) | (directions & FTSSH_WRITE ? POLLOUT : 0)), 0 }
    };
    int rc = poll(descriptors, 2, milliseconds);
    if(rc < 0 && errno != EINTR) return FTSSH_IO;
    if(descriptors[0].revents & POLLIN) drain_pipe(worker->control[0]);
    if((descriptors[0].revents | descriptors[1].revents) & POLLNVAL) return FTSSH_IO;
    return interactive_stop(worker);
}
static int interactive_text(const uint8_t *text, size_t length) {
    if(length && !text) return 0;
    for(size_t i = 0; i < length;) {
        uint8_t first = text[i++];
        if(first == 0) return 0;
        if(first < 0x80) continue;
        unsigned int following;
        uint8_t minimum = 0x80, maximum = 0xbf;
        if(first >= 0xc2 && first <= 0xdf) following = 1;
        else if(first >= 0xe0 && first <= 0xef) {
            following = 2;
            if(first == 0xe0) minimum = 0xa0;
            if(first == 0xed) maximum = 0x9f;
        } else if(first >= 0xf0 && first <= 0xf4) {
            following = 3;
            if(first == 0xf0) minimum = 0x90;
            if(first == 0xf4) maximum = 0x8f;
        } else return 0;
        if(following > length - i || text[i] < minimum || text[i] > maximum) return 0;
        i++;
        for(unsigned int n = 1; n < following; n++, i++)
            if(text[i] < 0x80 || text[i] > 0xbf) return 0;
    }
    return 1;
}
static int interactive_string(uint8_t **output, size_t *left, const void *text, size_t length) {
    if(*left < 4 || length > *left - 4 || !interactive_text(text, length)) return 0;
    store_u32(*output, (uint32_t)length);
    if(length) memcpy(*output + 4, text, length);
    *output += 4 + length; *left -= 4 + length; return 1;
}
static void interactive_callback(const char *name, int name_length, const char *instruction,
                                  int instruction_length, int prompt_count,
                                  const LIBSSH2_USERAUTH_KBDINT_PROMPT *prompts,
                                  LIBSSH2_USERAUTH_KBDINT_RESPONSE *responses, void **abstract) {
    ftssh_session *s = *abstract;
    struct interactive_worker *worker = s->interactive;
    int error = interactive_stop(worker);
    if(error) goto rejected;
    if(name_length < 0 || name_length > FTSSH_INTERACTIVE_TITLE_CAPACITY ||
       instruction_length < 0 || instruction_length > FTSSH_INTERACTIVE_INSTRUCTION_CAPACITY ||
       prompt_count < 0 || prompt_count > FTSSH_INTERACTIVE_PROMPT_CAPACITY ||
       (prompt_count && (!prompts || !responses)) || worker->round >= FTSSH_INTERACTIVE_ROUND_CAPACITY) {
        error = FTSSH_LIMIT; goto rejected;
    }
    pthread_mutex_lock(&worker->lock);
    uint8_t *output = worker->challenge; size_t left = sizeof(worker->challenge);
    int valid = interactive_string(&output, &left, name, (size_t)name_length) &&
                interactive_string(&output, &left, instruction, (size_t)instruction_length);
    if(valid && left >= 4) { store_u32(output, (uint32_t)prompt_count); output += 4; left -= 4; }
    else valid = 0;
    for(int i = 0; valid && i < prompt_count; i++) {
        if(prompts[i].length > FTSSH_INTERACTIVE_PROMPT_TEXT_CAPACITY ||
           (prompts[i].echo != 0 && prompts[i].echo != 1) || !left) { valid = 0; break; }
        *output++ = prompts[i].echo; left--;
        valid = interactive_string(&output, &left, prompts[i].text, prompts[i].length);
    }
    if(!valid) { pthread_mutex_unlock(&worker->lock); error = FTSSH_CHALLENGE; goto rejected; }
    worker->challenge_length = (size_t)(output - worker->challenge);
    worker->round++; worker->prompt_count = (uint32_t)prompt_count;
    worker->pending = 1; worker->supplied = 0;
    signal_pipe(worker->notification[1]);
    pthread_mutex_unlock(&worker->lock);
    for(;;) {
        error = interactive_stop(worker); if(error) goto rejected;
        pthread_mutex_lock(&worker->lock);
        if(worker->supplied) {
            /* Copy into libssh2-owned allocations only on its owning thread.
             * Its configured allocator releases these after sending a response. */
            const uint8_t *cursor = worker->responses + 4;
            size_t remaining = worker->response_length - 4;
            for(int i = 0; i < prompt_count; i++) {
                const uint8_t *answer; size_t length;
                if(!field(&cursor, &remaining, &answer, &length)) { error = FTSSH_CHALLENGE; break; }
                void *context = s;
                char *owned = allocate(length + 1, &context);
                if(!owned) { error = FTSSH_LIMIT; break; }
                if(length) memcpy(owned, answer, length);
                owned[length] = 0;
                responses[i].text = owned; responses[i].length = (unsigned int)length;
            }
            clear_interactive_responses(worker);
            worker->pending = 0; worker->supplied = 0;
            pthread_mutex_unlock(&worker->lock);
            if(error) goto rejected;
            return;
        }
        pthread_mutex_unlock(&worker->lock);
        error = interactive_wait(worker, 0); if(error) goto rejected;
    }
rejected:
    worker->callback_error = error;
    /* The void callback cannot defer or return a retry error. Prevent even an
     * empty response from being sent when this exact conversation is retired. */
    s->closing = 1;
}
static int interactive_result(ftssh_session *s, int rc) {
    if(s->socket_failed) return FTSSH_IO;
    switch(rc) {
    case LIBSSH2_ERROR_ALLOC: return FTSSH_LIMIT;
    case LIBSSH2_ERROR_SOCKET_NONE: case LIBSSH2_ERROR_SOCKET_SEND:
    case LIBSSH2_ERROR_SOCKET_DISCONNECT: case LIBSSH2_ERROR_SOCKET_RECV:
    case LIBSSH2_ERROR_BAD_SOCKET: return FTSSH_IO;
    case LIBSSH2_ERROR_METHOD_NONE: case LIBSSH2_ERROR_METHOD_NOT_SUPPORTED:
    case LIBSSH2_ERROR_ALGO_UNSUPPORTED: return FTSSH_UNSUPPORTED;
    default: return FTSSH_INTERACTIVE_FAILED;
    }
}
static void *interactive_run(void *argument) {
    struct interactive_worker *worker = argument;
    ftssh_session *s = worker->session;
    int answer = FTSSH_INTERACTIVE_FAILED;
    for(;;) {
        answer = interactive_stop(worker); if(answer) break;
        int rc = prepare_egress(s);
        if(rc < 0) { answer = rc; break; }
        if(rc == FTSSH_OK && !s->auth_methods_checked) {
            const char *methods = libssh2_userauth_list(s->ssh, s->user, (unsigned int)strlen(s->user));
            s->limited_receive = 0;
            if(libssh2_userauth_authenticated(s->ssh)) { answer = FTSSH_UNSUPPORTED; break; }
            if(methods) {
                answer = authentication_method(methods, "keyboard-interactive");
                if(answer) break;
                s->auth_methods_checked = 1;
            } else {
                rc = libssh2_session_last_errno(s->ssh);
                if(rc != LIBSSH2_ERROR_EAGAIN) { answer = interactive_result(s, rc); break; }
            }
        }
        if(rc == FTSSH_OK && s->auth_methods_checked) {
            s->receive_budget = 32768; s->limited_receive = 1;
            rc = libssh2_userauth_keyboard_interactive_ex(s->ssh, s->user, (unsigned int)strlen(s->user),
                                                         interactive_callback);
            s->limited_receive = 0;
            if(worker->callback_error) { answer = worker->callback_error; break; }
            answer = interactive_stop(worker); if(answer) break;
            if(rc == 0 && libssh2_userauth_authenticated(s->ssh)) {
                answer = worker->round ? FTSSH_OK : FTSSH_UNSUPPORTED;
                break;
            }
            if(rc != LIBSSH2_ERROR_EAGAIN) { answer = interactive_result(s, rc); break; }
        }
        int directions = transport_directions(s);
        if(directions & FTSSH_RETRY) continue;
        answer = interactive_wait(worker, directions ? directions : FTSSH_READ);
        if(answer) break;
    }
    pthread_mutex_lock(&worker->lock);
    clear_interactive_responses(worker);
    worker->pending = 0; worker->supplied = 0;
    worker->result = answer; worker->done = 1;
    signal_pipe(worker->notification[1]);
    pthread_mutex_unlock(&worker->lock);
    return NULL;
}
static void interactive_dispose(ftssh_session *s, struct interactive_worker *worker) {
    for(unsigned int i = 0; i < 2; i++) {
        if(worker->notification[i] >= 0) close(worker->notification[i]);
        if(worker->control[i] >= 0) close(worker->control[i]);
    }
    pthread_mutex_destroy(&worker->lock);
    clear_interactive_responses(worker);
    void *context = s; release(worker, &context);
    atomic_fetch_sub(&global_interactive_workers, 1);
}
static int start_interactive_worker(ftssh_session *s, const char *user, size_t user_length,
                                    uint64_t deadline) {
    int rc, previous_phase = s->phase;
    unsigned int count = atomic_load(&global_interactive_workers);
    do { if(count >= 2) return FTSSH_LIMIT; }
    while(!atomic_compare_exchange_weak(&global_interactive_workers, &count, count + 1));
    void *context = s;
    struct interactive_worker *worker = allocate(sizeof(*worker), &context);
    if(!worker) { atomic_fetch_sub(&global_interactive_workers, 1); return FTSSH_LIMIT; }
    memset(worker, 0, sizeof(*worker)); atomic_init(&worker->cancelled, 0);
    worker->notification[0] = worker->notification[1] = worker->control[0] = worker->control[1] = -1;
    if(pthread_mutex_init(&worker->lock, NULL)) {
        release(worker, &context); atomic_fetch_sub(&global_interactive_workers, 1); return FTSSH_LIMIT;
    }
    if(!owned_pipe(worker->notification) || !owned_pipe(worker->control)) {
        interactive_dispose(s, worker); return FTSSH_IO;
    }
    worker->session = s; worker->deadline_milliseconds = deadline;
    pthread_attr_t attributes;
    if(pthread_attr_init(&attributes)) { interactive_dispose(s, worker); return FTSSH_LIMIT; }
    rc = pthread_attr_setstacksize(&attributes, 512u * 1024u);
    if(!rc) {
        if(user != s->user) memcpy(s->user, user, user_length);
        s->user[user_length] = 0;
        s->phase = AUTH; s->interactive = worker;
        for(ftssh_session *p = s; p; p = p->parent) p->worker_owner = worker;
        rc = pthread_create(&worker->thread, &attributes, interactive_run, worker);
    }
    pthread_attr_destroy(&attributes);
    if(rc) {
        s->phase = previous_phase; s->interactive = NULL;
        for(ftssh_session *p = s; p; p = p->parent) p->worker_owner = NULL;
        interactive_dispose(s, worker); return FTSSH_LIMIT;
    }
    return FTSSH_OK;
}
int ftssh_begin_interactive_authentication(ftssh_session *s, const char *user, size_t user_length,
                                         uint32_t timeout_milliseconds) {
    int rc = check(s, APPROVED); if(rc) return rc;
    if(!user || !user_length || user_length > 256 || !timeout_milliseconds || timeout_milliseconds > 120000)
        return FTSSH_INVALID;
    for(size_t i = 0; i < user_length; i++) if((unsigned char)user[i] < 32 || user[i] == 127) return FTSSH_INVALID;
    uint64_t now = monotonic_milliseconds(); if(!now) return FTSSH_IO;
    return start_interactive_worker(s, user, user_length, now + timeout_milliseconds);
}
int ftssh_continue_interactive_authentication(ftssh_session *s) {
    int rc = check(s, PARTIAL_AUTH); if(rc) return rc;
    if(!s->publickey_interactive || !s->partial_ready || !s->auth_methods_checked) return FTSSH_STATE;
    rc = publickey_deadline(s); if(rc) return rc;
    /* The signed failure's method list already proves KI is available. A new
     * "none" request would be a different attempt and could reset the factor. */
    s->partial_ready = 0;
    rc = start_interactive_worker(s, s->user, strlen(s->user), s->authentication_deadline);
    return rc ? fail(s, rc) : FTSSH_OK;
}
int ftssh_copy_interactive_challenge(ftssh_session *s, uint8_t *output, size_t capacity,
                                   size_t *length, uint32_t *round) {
    if(!s || !output || !length || !round) return FTSSH_INVALID;
    *length = 0; *round = 0;
    struct interactive_worker *worker = s->interactive; if(!worker) return FTSSH_STATE;
    pthread_mutex_lock(&worker->lock);
    int rc = FTSSH_STATE;
    if(worker->pending && !worker->supplied && !worker->done && !interactive_stop(worker)) {
        if(worker->challenge_length > capacity) rc = FTSSH_LIMIT;
        else {
            memcpy(output, worker->challenge, worker->challenge_length);
            *length = worker->challenge_length; *round = worker->round; rc = FTSSH_OK;
        }
    }
    pthread_mutex_unlock(&worker->lock); return rc;
}
int ftssh_supply_interactive_responses(ftssh_session *s, uint32_t round, const uint8_t *responses, size_t length) {
    if(!s || !responses || length < 4 || length > FTSSH_INTERACTIVE_ENCODED_RESPONSES_CAPACITY) return FTSSH_INVALID;
    struct interactive_worker *worker = s->interactive; if(!worker) return FTSSH_STATE;
    uint32_t count = (uint32_t)responses[0] << 24 | (uint32_t)responses[1] << 16 | (uint32_t)responses[2] << 8 | responses[3];
    if(count > FTSSH_INTERACTIVE_PROMPT_CAPACITY) return FTSSH_INVALID;
    const uint8_t *cursor = responses + 4; size_t left = length - 4;
    for(uint32_t i = 0; i < count; i++) {
        const uint8_t *value; size_t n;
        if(!field(&cursor, &left, &value, &n) || n > FTSSH_INTERACTIVE_RESPONSE_CAPACITY || !interactive_text(value, n))
            return FTSSH_INVALID;
    }
    if(left) return FTSSH_INVALID;
    pthread_mutex_lock(&worker->lock);
    int rc = FTSSH_STATE;
    if(worker->pending && !worker->supplied && !worker->done && !interactive_stop(worker) &&
       worker->round == round && worker->prompt_count == count) {
        memcpy(worker->responses, responses, length); worker->response_length = length; worker->supplied = 1;
        signal_pipe(worker->control[1]); rc = FTSSH_OK;
    }
    pthread_mutex_unlock(&worker->lock); return rc;
}
int ftssh_cancel_interactive_authentication(ftssh_session *s) {
    if(!s) return FTSSH_INVALID;
    struct interactive_worker *worker = s->interactive;
    if(worker && !atomic_exchange(&worker->cancelled, 1)) signal_pipe(worker->control[1]);
    return FTSSH_OK;
}
int ftssh_finish_interactive_authentication(ftssh_session *s) {
    if(!s) return FTSSH_INVALID;
    struct interactive_worker *worker = s->interactive; if(!worker) return FTSSH_STATE;
    if(pthread_join(worker->thread, NULL)) return FTSSH_STATE;
    int answer = atomic_load(&worker->cancelled) ? FTSSH_CLOSED : worker->result;
    for(ftssh_session *p = s; p; p = p->parent) p->worker_owner = NULL;
    s->interactive = NULL;
    interactive_dispose(s, worker);
    if(answer == FTSSH_OK) { s->phase = AUTHENTICATED; return FTSSH_OK; }
    return fail(s, answer);
}
int ftssh_authentication_step(ftssh_session *s) {
    if(s && s->interactive) {
        struct interactive_worker *worker = s->interactive;
        pthread_mutex_lock(&worker->lock);
        drain_pipe(worker->notification[0]);
        int status = worker->done ? FTSSH_INTERACTIVE_FINISHED :
            (worker->pending && !worker->supplied && !interactive_stop(worker) ? FTSSH_INTERACTIVE_NEEDED : FTSSH_AGAIN);
        pthread_mutex_unlock(&worker->lock); return status;
    }
    int rc = check(s, AUTH); if(rc) return rc;
    rc = publickey_deadline(s); if(rc) return rc;
    rc = prepare_egress(s); if(rc) return rc;
    if(!s->auth_methods_checked) {
        s->receive_budget = 32768; s->limited_receive = 1;
        const char *methods = libssh2_userauth_list(s->ssh, s->user, (unsigned int)strlen(s->user));
        s->limited_receive = 0;
        int authenticated = libssh2_userauth_authenticated(s->ssh);
        if(!methods || authenticated || s->socket_failed) {
            int error = libssh2_session_last_errno(s->ssh);
            return s->password_authentication ? password_authentication_result(s, error, authenticated, 0)
                                               : authentication_result(s, error, authenticated, 0);
        }
        rc = authentication_method(methods, s->password_authentication ? "password" : "publickey");
        if(rc) return fail(s, rc);
        s->auth_methods_checked = 1;
    }
    if(s->password_authentication) {
        if(!s->password_supplied) return FTSSH_PASSWORD_NEEDED;
        s->receive_budget = 32768; s->limited_receive = 1;
        rc = libssh2_userauth_password_ex(s->ssh, s->user, (unsigned int)strlen(s->user),
                                         (const char *)s->password, (unsigned int)s->password_length, NULL);
        s->limited_receive = 0;
        return password_authentication_result(s, rc, libssh2_userauth_authenticated(s->ssh), 1);
    }
    if(s->challenge_length && !s->signature_length) return FTSSH_SIGNATURE_NEEDED;
    void *context = s;
    s->receive_budget = 32768; s->limited_receive = 1;
    rc = libssh2_userauth_publickey(s->ssh, s->user, s->public_blob, s->public_length, sign_callback, &context);
    s->limited_receive = 0;
    return authentication_result(s, rc, libssh2_userauth_authenticated(s->ssh), 1);
}
int ftssh_copy_challenge(ftssh_session *s, uint8_t *out, size_t capacity, size_t *length) {
    int rc = check(s, AUTH); if(rc) return rc;
    rc = publickey_deadline(s); if(rc) return rc;
    if(!out || !length) return FTSSH_INVALID;
    *length = 0;
    if(!s->challenge_length || s->signature_consumed) return FTSSH_STATE;
    if(s->challenge_length > capacity) return FTSSH_LIMIT;
    memcpy(out, s->challenge, s->challenge_length); *length = s->challenge_length; return FTSSH_OK;
}
int ftssh_supply_signature(ftssh_session *s, const uint8_t *blob, size_t length) {
    int rc = check(s, AUTH); if(rc) return rc;
    rc = publickey_deadline(s); if(rc) return rc;
    if(!blob || !length || length > FTSSH_SIGNATURE_CAPACITY) return FTSSH_INVALID;
    if(!s->challenge_length || s->signature_length || s->signature_consumed) return FTSSH_STATE;
    const uint8_t *cursor = blob, *payload; size_t left = length, n;
    if(!expected_field(&cursor, &left, s->signature_algorithm, strlen(s->signature_algorithm)) ||
       !field(&cursor, &left, &payload, &n) || left || !n || n > sizeof(s->signature)) return FTSSH_CHALLENGE;
    if(!signature_payload(s->algorithm, s->rsa_signature_length, payload, n)) return FTSSH_CHALLENGE;
    memcpy(s->signature, payload, n); s->signature_length = n; return FTSSH_OK;
}
static void store_u32(uint8_t *out, uint32_t value) {
    out[0] = (uint8_t)(value >> 24); out[1] = (uint8_t)(value >> 16);
    out[2] = (uint8_t)(value >> 8); out[3] = (uint8_t)value;
}
int ftssh_begin_forward(ftssh_session *s, const char *host, size_t length, uint16_t port) {
    int rc = check(s, AUTHENTICATED); if(rc) return rc;
    if(s->forwarding_depth >= FTSSH_MAX_JUMP_HOPS) return FTSSH_UNSUPPORTED;
    if(!host || !length || length > 253 || !port) return FTSSH_INVALID;
    for(size_t i = 0; i < length; i++) {
        unsigned char c = (unsigned char)host[i];
        if(!((c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '.' || c == '-' || c == ':'))
            return FTSSH_INVALID;
    }
    /* RFC 4254 direct-tcpip fields, with an inert loopback origin. The channel
     * uses the same bounded window/packet sizes as our existing terminal. */
    uint8_t *out = s->forward_request;
    store_u32(out, (uint32_t)length); memcpy(out + 4, host, length); out += 4 + length;
    store_u32(out, port); out += 4;
    store_u32(out, 9); memcpy(out + 4, "127.0.0.1", 9); out += 13;
    store_u32(out, 0); out += 4;
    s->forward_length = (size_t)(out - s->forward_request);
    s->phase = FORWARD_OPENING; return FTSSH_OK;
}
int ftssh_forward_step(ftssh_session *s) {
    int rc = check(s, FORWARD_OPENING); if(rc) return rc;
    rc = prepare_egress(s); if(rc) return rc;
    s->receive_budget = 32768; s->limited_receive = 1;
    s->channel = libssh2_channel_open_ex(s->ssh, "direct-tcpip", 12, 128 * 1024, 16384,
                                        (const char *)s->forward_request, (unsigned int)s->forward_length);
    s->limited_receive = 0;
    if(!s->channel) return libssh2_session_last_errno(s->ssh) == LIBSSH2_ERROR_EAGAIN ? FTSSH_AGAIN : fail(s, FTSSH_CHANNEL);
    rc = libssh2_channel_handle_extended_data2(s->channel, LIBSSH2_CHANNEL_EXTENDED_DATA_IGNORE);
    if(rc) return fail(s, FTSSH_CHANNEL);
    s->phase = FORWARD_READY; return FTSSH_OK;
}
int ftssh_forward_adopt(ftssh_session **outer, size_t maximum, ftssh_session **inner) {
    if(!outer || !*outer || !inner || outer == inner) return FTSSH_INVALID;
    *inner = NULL;
    ftssh_session *p = *outer;
    int rc = check(p, FORWARD_READY); if(rc) return rc;
    if(p->forwarding_depth >= FTSSH_MAX_JUMP_HOPS) return FTSSH_UNSUPPORTED;
    ftssh_session *s = NULL;
    /* libssh2 requires a real descriptor for nonblocking-state bookkeeping.
     * It is only a readiness identity for the inner library: callbacks below
     * route every byte through p->channel, never directly through this fd. */
    rc = ftssh_create(p->fd, maximum, &s);
    if(rc) return rc;
    s->parent = p; s->forwarding_depth = p->forwarding_depth + 1;
    *outer = NULL; *inner = s;
    return FTSSH_OK;
}
int ftssh_begin_shell(ftssh_session *s, uint32_t c, uint32_t r, uint32_t w, uint32_t h) {
    int rc = check(s, AUTHENTICATED); if(rc) return rc;
    if(!valid_geometry(c, r, w, h)) return FTSSH_INVALID;
    s->geometry[0] = c; s->geometry[1] = r; s->geometry[2] = w; s->geometry[3] = h;
    s->phase = OPENING; return FTSSH_OK;
}
int ftssh_begin_command(ftssh_session *s, const char *command, size_t length) {
    int rc = check(s, AUTHENTICATED); if(rc) return rc;
    if(!command || !length || length > FTSSH_COMMAND_CAPACITY) return FTSSH_INVALID;
    for(size_t i = 0; i < length; i++) if((unsigned char)command[i] < 32 || (unsigned char)command[i] > 126) return FTSSH_INVALID;
    memcpy(s->command, command, length); s->command[length] = 0; s->command_length = length;
    s->phase = OPENING; return FTSSH_OK;
}
int ftssh_shell_step(ftssh_session *s) {
    if(!s) return FTSSH_INVALID;
    if(s->worker_owner) return FTSSH_STATE;
    int rc = prepare_egress(s); if(rc) return rc;
    if(s->phase == OPENING) {
        s->channel = libssh2_channel_open_ex(s->ssh, "session", 7, 128 * 1024, 16384, NULL, 0);
        if(!s->channel) return libssh2_session_last_errno(s->ssh) == LIBSSH2_ERROR_EAGAIN ? FTSSH_AGAIN : fail(s, FTSSH_CHANNEL);
        rc = libssh2_channel_handle_extended_data2(s->channel, s->command_length ?
            LIBSSH2_CHANNEL_EXTENDED_DATA_IGNORE : LIBSSH2_CHANNEL_EXTENDED_DATA_MERGE);
        if(rc) return fail(s, FTSSH_CHANNEL);
        s->phase = s->command_length ? SHELL : PTY;
    }
    if(s->phase == PTY) {
        rc = result(s, libssh2_channel_request_pty_ex(s->channel, "xterm-256color", 14, NULL, 0,
            (int)s->geometry[0], (int)s->geometry[1], (int)s->geometry[2], (int)s->geometry[3]), FTSSH_CHANNEL);
        if(rc) return rc;
        s->phase = SHELL;
    }
    if(s->phase == SHELL) {
        rc = result(s, s->command_length ?
            libssh2_channel_process_startup(s->channel, "exec", 4, s->command, (unsigned int)s->command_length) :
            libssh2_channel_process_startup(s->channel, "shell", 5, NULL, 0), FTSSH_CHANNEL);
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
static int finish_write_receipt(ftssh_session *s) {
    int progress = 0;
    if(drain_egress(s, &progress) < 0) return fail(s, FTSSH_IO);
    if(!egress_empty(s)) return FTSSH_AGAIN;
    /* Preserve the existing physical handoff boundary: libssh2 accepting a
     * plaintext packet into ciphertext queues is not yet a completed write. */
    s->written += s->write_receipt; s->write_offset += s->write_receipt;
    s->write_receipt = 0;
    if(s->write_offset == s->write_length) {
        memset(s->write_buffer, 0, s->write_length); s->write_offset = 0; s->write_length = 0;
        return FTSSH_OK;
    }
    return FTSSH_AGAIN;
}
int ftssh_flush_write(ftssh_session *s) {
    int rc = check(s, LIVE); if(rc) return rc;
    rc = prepare_egress(s); if(rc) return rc;
    if(s->write_receipt || !s->write_length) return finish_write_receipt(s);
    size_t left = s->write_length - s->write_offset;
    s->limited_receive = 1;
    ssize_t count = libssh2_channel_write_ex(s->channel, 0, (const char *)s->write_buffer + s->write_offset, left);
    s->limited_receive = 0;
    if(count == LIBSSH2_ERROR_EAGAIN || count == 0) return FTSSH_AGAIN;
    if(count < 0 || (size_t)count > left) return fail(s, FTSSH_IO);
    s->write_receipt = (size_t)count;
    return finish_write_receipt(s);
}
int ftssh_read(ftssh_session *s, uint8_t *out, size_t capacity, size_t *length) {
    int rc = check(s, LIVE); if(rc) return rc;
    if(!out || !length || !capacity || capacity > 16384) return FTSSH_INVALID;
    *length = 0;
    int progress = 0;
    if(drain_egress(s, &progress) < 0) return fail(s, FTSSH_IO);
    s->receive_budget = 32768; s->limited_receive = 1;
    ssize_t count = libssh2_channel_read_ex(s->channel, 0, (char *)out, capacity);
    s->limited_receive = 0;
    if(count == LIBSSH2_ERROR_EAGAIN) return FTSSH_AGAIN;
    if(count < 0 || (size_t)count > capacity) return fail(s, FTSSH_IO);
    if(count == 0) {
        if(!libssh2_channel_eof(s->channel)) return FTSSH_AGAIN;
        s->output_eof = 1; return FTSSH_EOF;
    }
    *length = (size_t)count; return FTSSH_OK;
}
static int copy_completion(ftssh_session *s, ftssh_completion *out) {
    size_t length = 0;
    int rc = libssh2_channel_get_exit_signal(s->channel, NULL, &length, NULL, NULL, NULL, NULL);
    if(rc) return fail(s, FTSSH_IO);
    if(length && length < sizeof(out->signal)) {
        char *name = NULL;
        rc = libssh2_channel_get_exit_signal(s->channel, &name, &length, NULL, NULL, NULL, NULL);
        if(rc || !name) return fail(s, FTSSH_IO);
        int printable = 1;
        for(size_t i = 0; i < length; i++) {
            if((unsigned char)name[i] < 33 || (unsigned char)name[i] > 126) printable = 0;
        }
        if(printable) {
            memcpy(out->signal, name, length); out->signal[length] = 0;
            out->kind = FTSSH_EXIT_SIGNAL;
        }
        libssh2_free(s->ssh, name);
    }
    uint32_t status = (uint32_t)libssh2_channel_get_exit_status(s->channel);
    if(out->kind == FTSSH_EXIT_UNKNOWN && status != 0) {
        out->kind = FTSSH_EXIT_STATUS; out->exit_status = status;
    }
    return FTSSH_OK;
}
int ftssh_completion_step(ftssh_session *s, ftssh_completion *out) {
    int rc = check(s, LIVE); if(rc) return rc;
    if(!out) return FTSSH_INVALID;
    memset(out, 0, sizeof(*out));
    if(!s->output_eof) return FTSSH_STATE;
    /* wait_closed otherwise reads until EAGAIN; cap each call even if a hostile
     * peer continuously sends control traffic. No new buffer or timer is owned
     * here. Existing libssh2 allocation limits also bound already buffered data. */
    s->limited_receive = 1; s->receive_budget = 32768;
    rc = libssh2_channel_wait_closed(s->channel);
    int exhausted = s->receive_budget == 0;
    s->limited_receive = 0;
    if(rc != 0 && rc != LIBSSH2_ERROR_EAGAIN) return fail(s, FTSSH_IO);
    int copied = copy_completion(s, out); if(copied) return copied;
    if(rc == 0) { out->channel_closed = 1; return FTSSH_OK; }
    return exhausted ? FTSSH_YIELD : FTSSH_AGAIN;
}
int ftssh_resize(ftssh_session *s, uint32_t c, uint32_t r, uint32_t w, uint32_t h) {
    int rc = check(s, LIVE); if(rc) return rc;
    if(s->command_length) return FTSSH_STATE;
    if(!valid_geometry(c, r, w, h)) return FTSSH_INVALID;
    uint32_t requested[4] = {c, r, w, h};
    if(s->write_length) return FTSSH_AGAIN;
    if(s->resize_pending && memcmp(s->resize_geometry, requested, sizeof(requested))) return FTSSH_STATE;
    rc = prepare_egress(s); if(rc) return rc;
    memcpy(s->resize_geometry, requested, sizeof(requested));
    if(s->resize_pending != 2) {
        s->resize_pending = 1;
        rc = result(s, libssh2_channel_request_pty_size_ex(s->channel, (int)c, (int)r, (int)w, (int)h), FTSSH_CHANNEL);
        if(rc) return rc;
        s->resize_pending = 2;
    }
    int progress = 0;
    if(drain_egress(s, &progress) < 0) return fail(s, FTSSH_IO);
    if(!egress_empty(s)) return FTSSH_AGAIN;
    memcpy(s->geometry, requested, sizeof(requested)); s->resize_pending = 0;
    return FTSSH_OK;
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
    rc = prepare_egress(s); if(rc) return rc;
    if(!egress_empty(s)) return FTSSH_AGAIN;
    /* libssh2 binds a partly sent packet to the caller's buffer and refuses any
     * other packet until that exact buffer is resent; its keepalive buffer is
     * its own stack frame. Send only when nothing is pending and the whole
     * packet fits in the socket buffer, so it cannot be left half sent. */
    if(s->write_length || s->resize_pending ||
       (libssh2_session_block_directions(s->ssh) & LIBSSH2_SESSION_BLOCK_OUTBOUND)) return FTSSH_AGAIN;
    for(ftssh_session *p = s->parent; p; p = p->parent) {
        if(p->closing || p->phase != FORWARD_READY || p->socket_failed) return fail(s, FTSSH_IO);
        if((libssh2_session_block_directions(p->ssh) & LIBSSH2_SESSION_BLOCK_OUTBOUND) ||
           libssh2_channel_window_write_ex(p->channel, NULL) < 4096) return FTSSH_AGAIN;
    }
    int capacity = 0, queued = unsent_socket_bytes(s->fd);
    socklen_t length = sizeof(capacity);
    if(queued < 0 || getsockopt(s->fd, SOL_SOCKET, SO_SNDBUF, &capacity, &length)) return FTSSH_UNSUPPORTED;
    if(capacity - queued < 4096 * (int)(s->forwarding_depth + 1)) return FTSSH_AGAIN;
    int next = 0;
    uint64_t sent = s->sent;
    if(libssh2_keepalive_send(s->ssh, &next)) return fail(s, FTSSH_IO);
    int progress = 0;
    if(drain_egress(s, &progress) < 0 || !egress_empty(s)) return fail(s, FTSSH_IO);
    /* Still pending despite the room checked above: fail rather than block input. */
    for(ftssh_session *p = s; p; p = p->parent) {
        if(libssh2_session_block_directions(p->ssh) & LIBSSH2_SESSION_BLOCK_OUTBOUND) return fail(s, FTSSH_IO);
    }
    /* libssh2 skips a keepalive within 2 s of its last (by whole seconds of
     * wall time) and still reports success: nothing left, no reply is due. */
    if(s->sent == sent) return FTSSH_AGAIN;
    return FTSSH_OK;
}
static int transport_directions(ftssh_session *s) {
    if(!s || !s->ssh || s->phase >= FAILED) return 0;
    int progress = 0;
    if(drain_egress(s, &progress) < 0) { fail(s, FTSSH_IO); return FTSSH_RETRY; }
    /* Inner write may need an outer WINDOW_ADJUST read, and inner read may
     * need an outer packet write. Waiting on the inner hint would busy-spin. */
    ftssh_session *root = s;
    int retry = progress || ((s->phase == AUTH || s->phase == FORWARD_OPENING) && s->receive_budget == 0);
    while(root->parent) {
        root = root->parent;
        if(root->receive_budget == 0) retry = 1;
    }
    int directions = libssh2_session_block_directions(root->ssh);
    if(root->egress_count) directions |= LIBSSH2_SESSION_BLOCK_OUTBOUND;
    return (directions & LIBSSH2_SESSION_BLOCK_INBOUND ? FTSSH_READ : 0) |
           (directions & LIBSSH2_SESSION_BLOCK_OUTBOUND ? FTSSH_WRITE : 0) |
           (retry ? FTSSH_RETRY : 0);
}
int ftssh_directions(ftssh_session *s) {
    if(s && s->interactive) return FTSSH_READ;
    if(s && s->worker_owner) return 0;
    return transport_directions(s);
}
int ftssh_descriptor(ftssh_session *s) {
    return s ? (s->interactive ? s->interactive->notification[0] : s->fd) : -1;
}
int ftssh_get_stats(ftssh_session *s, ftssh_stats *out) {
    if(!s || !out) return FTSSH_INVALID;
    if(s->worker_owner) return FTSSH_STATE;
    *out = (ftssh_stats){s->allocated, s->peak, s->write_length - s->write_offset, s->accepted, s->written,
                         s->terminal_error, s->received};
    for(ftssh_session *p = s->parent; p; p = p->parent) {
        out->allocated_bytes += p->allocated;
        /* Sum of independently measured peaks is a conservative chain bound. */
        out->peak_allocated_bytes += p->peak;
    }
    return FTSSH_OK;
}
int ftssh_close(ftssh_session *s) {
    if(!s) return FTSSH_INVALID;
    if(s->interactive) { ftssh_cancel_interactive_authentication(s); return FTSSH_AGAIN; }
    if(s->worker_owner) return FTSSH_STATE;
    if(s->phase == CLOSED) return FTSSH_OK;
    /* Force callback failure even if libssh2 was mid-EAGAIN. No teardown packet
     * can wait for peer cooperation and no stale fd can be used after close. */
    s->closing = 1;
    clear_password(s);
    s->egress_head = 0; s->egress_count = 0; s->egress_flight = 0; s->write_receipt = 0;
    if(s->parent) {
        int rc = ftssh_close(s->parent);
        s->fd = -1;
        if(rc) return fail(s, rc);
    } else if(s->fd >= 0) { shutdown(s->fd, SHUT_RDWR); close(s->fd); s->fd = -1; }
    if(s->ssh) {
        int rc = libssh2_session_free(s->ssh);
        if(rc != 0) return fail(s, FTSSH_IO); // retain ownership for explicit retry
        s->ssh = NULL; s->channel = NULL;
    }
    if(s->egress) {
        memset(s->egress, 0, EGRESS_CAPACITY);
        void *context = s; release(s->egress, &context); s->egress = NULL;
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
    rc = ftssh_destroy(&(*address)->parent); if(rc) return rc;
    free(*address); *address = NULL; atomic_fetch_sub(&global_sessions, 1); return FTSSH_OK;
}
