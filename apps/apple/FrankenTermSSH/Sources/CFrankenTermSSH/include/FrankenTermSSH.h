#ifndef FRANKENTERM_SSH_H
#define FRANKENTERM_SSH_H
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Native libssh2 ownership boundary. This does not implement SSH or a terminal.
 * One serial owner calls a handle; no function may overlap another on that handle.
 * No pointer into libssh2 crosses this API. All outputs are bounded copies.
 * The caller owns socket connection, deadlines, readiness and workspace policy.
 * Authentication and creation of a remote shell are separate deliberate steps.
 */
typedef struct ftssh_session ftssh_session;
enum {
    FTSSH_OK = 0, FTSSH_AGAIN = 1, FTSSH_SIGNATURE_NEEDED = 2, FTSSH_EOF = 3,
    FTSSH_INVALID = -1000, FTSSH_STATE = -1001, FTSSH_LIMIT = -1002,
    FTSSH_IO = -1003, FTSSH_NEGOTIATION = -1004, FTSSH_HOST_REJECTED = -1005,
    FTSSH_AUTH_REJECTED = -1006, FTSSH_CHALLENGE = -1007, FTSSH_CHANNEL = -1008,
    FTSSH_CLOSED = -1009, FTSSH_UNSUPPORTED = -1010
};
enum { FTSSH_READ = 1, FTSSH_WRITE = 2, FTSSH_WRITE_CAPACITY = 16384 };

typedef struct {
    size_t allocated_bytes, peak_allocated_bytes, pending_write_bytes;
    uint64_t accepted_bytes, channel_accepted_bytes;
    int terminal_error;
} ftssh_stats;

/* On success consumes this connected SOCK_STREAM descriptor. On failure leaves
 * it owned by the caller, but may have set O_NONBLOCK/FD_CLOEXEC/SO_NOSIGPIPE.
 * The descriptor must have no other concurrent reader/writer or shared owner.
 * max_allocated bounds libssh2 allocations, not OpenSSL/system socket memory.
 */
int ftssh_create(int connected_fd, size_t max_allocated, ftssh_session **result);
int ftssh_handshake(ftssh_session *session);
int ftssh_copy_host_key(ftssh_session *session, uint8_t *output, size_t capacity, size_t *length);
int ftssh_copy_method(ftssh_session *session, int method, char *output, size_t capacity);
/* Call ONLY after authenticating this exact key under the intended endpoint's
 * current host policy. An equal key does not itself constitute a trust decision.
 * No authentication request is permitted before this explicit approval.
 */
int ftssh_approve_host_key(ftssh_session *session, const uint8_t *key, size_t length);

/* One chosen public identity per connection; no password, agent enumeration,
 * automatic fallback, or private-key material enters this adapter. */
int ftssh_begin_authentication(ftssh_session *session, const char *user, size_t user_length,
                             const uint8_t *public_blob, size_t blob_length);
int ftssh_authentication_step(ftssh_session *session);
/* Challenge is the exact bounded RFC 4252 preimage produced by libssh2. Its
 * service, method, user, public key and algorithm were checked before retention.
 * The signer must construct/compare its typed challenge, not offer a general
 * signing oracle. EAGAIN reentry must present the exact same preimage.
 */
int ftssh_copy_challenge(ftssh_session *session, uint8_t *output, size_t capacity, size_t *length);
/* Takes a full SSH signature blob (algorithm string + signature string), not a
 * packet or DER ECDSA signature. It unwraps the payload for libssh2 exactly once.
 */
int ftssh_supply_signature(ftssh_session *session, const uint8_t *blob, size_t length);

int ftssh_begin_shell(ftssh_session *session, uint32_t columns, uint32_t rows,
                      uint32_t pixel_width, uint32_t pixel_height);
int ftssh_shell_step(ftssh_session *session);
/* Admission copies the whole bounded unit or accepts nothing. Success means
 * accepted into this adapter's single 16 KiB slot, NOT remote delivery. */
int ftssh_admit_write(ftssh_session *session, const uint8_t *bytes, size_t length);
/* Retains the exact write pointer and length across libssh2 EAGAIN. */
int ftssh_flush_write(ftssh_session *session);
int ftssh_read(ftssh_session *session, uint8_t *output, size_t capacity, size_t *length);
/* No acknowledged geometry exists in ordinary SSH. Success means the local
 * window-change request completed. Resume the same request after AGAIN. */
int ftssh_resize(ftssh_session *session, uint32_t columns, uint32_t rows,
                  uint32_t pixel_width, uint32_t pixel_height);
int ftssh_directions(ftssh_session *session);
int ftssh_descriptor(ftssh_session *session);
int ftssh_get_stats(ftssh_session *session, ftssh_stats *result);
/* Aborts this one owned SSH session. No remote process-survival claim. close is
 * idempotent, interrupts I/O and releases native state without network waits.
 * destroy releases the handle only after close has completed successfully. */
int ftssh_close(ftssh_session *session);
int ftssh_destroy(ftssh_session **session);

#ifdef __cplusplus
}
#endif
#endif
