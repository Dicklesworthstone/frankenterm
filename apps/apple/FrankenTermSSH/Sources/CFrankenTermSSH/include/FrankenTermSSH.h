#ifndef FRANKENTERM_SSH_H
#define FRANKENTERM_SSH_H
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Native libssh2 ownership boundary. This does not implement SSH or a terminal.
 * One serial owner calls a handle; no function may overlap another on that handle.
 * During keyboard-interactive authentication the bounded native worker owns all
 * libssh2 access. Only its documented control/snapshot APIs may run until joined.
 * No pointer into libssh2 crosses this API. All outputs are bounded copies.
 * The caller owns socket connection, deadlines, readiness and workspace policy.
 * Authentication and creation of a remote shell are separate deliberate steps.
 */
typedef struct ftssh_session ftssh_session;
enum {
    FTSSH_OK = 0, FTSSH_AGAIN = 1, FTSSH_SIGNATURE_NEEDED = 2, FTSSH_EOF = 3,
    FTSSH_YIELD = 4, FTSSH_PASSWORD_NEEDED = 5,
    FTSSH_INTERACTIVE_NEEDED = 6, FTSSH_INTERACTIVE_FINISHED = 7,
    FTSSH_AUTH_PARTIAL_REQUIRED = 8,
    FTSSH_INVALID = -1000, FTSSH_STATE = -1001, FTSSH_LIMIT = -1002,
    FTSSH_IO = -1003, FTSSH_NEGOTIATION = -1004, FTSSH_HOST_REJECTED = -1005,
    FTSSH_AUTH_REJECTED = -1006, FTSSH_CHALLENGE = -1007, FTSSH_CHANNEL = -1008,
    FTSSH_CLOSED = -1009, FTSSH_UNSUPPORTED = -1010,
    FTSSH_AUTH_FAILED = -1011, FTSSH_PASSWORD_FAILED = -1012,
    FTSSH_INTERACTIVE_FAILED = -1013, FTSSH_AUTH_TIMEOUT = -1014
};
enum { FTSSH_READ = 1, FTSSH_WRITE = 2, FTSSH_RETRY = 4,
       FTSSH_WRITE_CAPACITY = 16384, FTSSH_COMMAND_CAPACITY = 1024,
       FTSSH_MAX_JUMP_HOPS = 3, FTSSH_PASSWORD_CAPACITY = 1024,
       /* Keep raw-key admission distinct from a bounded certificate offer.
        * 32 KiB plus account/algorithm/signature framing fits libssh2's
        * 34744-byte outbound payload ceiling. */
       FTSSH_RAW_PUBLIC_KEY_CAPACITY = 1053,
       FTSSH_PUBLIC_KEY_CAPACITY = 32768, FTSSH_CHALLENGE_CAPACITY = 33792,
       FTSSH_SIGNATURE_CAPACITY = 1044,
       FTSSH_INTERACTIVE_ROUND_CAPACITY = 4, FTSSH_INTERACTIVE_PROMPT_CAPACITY = 8,
       FTSSH_INTERACTIVE_TITLE_CAPACITY = 256, FTSSH_INTERACTIVE_INSTRUCTION_CAPACITY = 1024,
       FTSSH_INTERACTIVE_PROMPT_TEXT_CAPACITY = 1024, FTSSH_INTERACTIVE_CHALLENGE_CAPACITY = 8192,
       FTSSH_INTERACTIVE_RESPONSE_CAPACITY = 1024, FTSSH_INTERACTIVE_ENCODED_RESPONSES_CAPACITY = 8228 };

typedef struct {
    size_t allocated_bytes, peak_allocated_bytes, pending_write_bytes;
    uint64_t accepted_bytes, channel_accepted_bytes;
    int terminal_error;
    /* Bytes read from the server's socket, including SSH-level replies. */
    uint64_t received_bytes;
} ftssh_stats;

enum { FTSSH_EXIT_UNKNOWN = 0, FTSSH_EXIT_STATUS = 1, FTSSH_EXIT_SIGNAL = 2 };
typedef struct {
    int kind, channel_closed;
    uint32_t exit_status;
    char signal[65];
} ftssh_completion;

/* On success consumes this connected SOCK_STREAM descriptor. On failure leaves
 * it owned by the caller, but may have set O_NONBLOCK/FD_CLOEXEC/SO_NOSIGPIPE.
 * The descriptor must have no other concurrent reader/writer or shared owner.
 * max_allocated bounds libssh2 allocations and the owned 64 KiB encrypted-output
 * queue, not OpenSSL/system socket memory. A budget too small for initialization
 * fails without consuming the descriptor.
 */
int ftssh_create(int connected_fd, size_t max_allocated, ftssh_session **result);
/* FTSSH_NEGOTIATION: the server answered, and no exchange both accept was
 * agreed. A transport that failed or closed first, with no disconnect message
 * from the server, is FTSSH_IO. */
int ftssh_handshake(ftssh_session *session);
int ftssh_copy_host_key(ftssh_session *session, uint8_t *output, size_t capacity, size_t *length);
int ftssh_copy_method(ftssh_session *session, int method, char *output, size_t capacity);
/* Call ONLY after authenticating this exact key under the intended endpoint's
 * current host policy. An equal key does not itself constitute a trust decision.
 * No authentication request is permitted before this explicit approval.
 */
int ftssh_approve_host_key(ftssh_session *session, const uint8_t *key, size_t length);

/* One chosen public identity per connection; no agent enumeration, automatic
 * fallback or private-key material enters this adapter. A user certificate must
 * already be verified against the selected raw key, account, validity and
 * permissions by the caller. This bridge validates its bounded wire structure
 * and exact offer; it does not establish issuer trust or evaluate a KRL.
 * RSA public blobs retain their canonical format, but only SHA-2 challenges
 * (including the corresponding certificate request names) reach a signer.
 * An unsigned RSA refusal is terminal, not proof for trying another identity:
 * libssh2 cannot expose the offered algorithm before its signing callback. */
int ftssh_begin_authentication(ftssh_session *session, const char *user, size_t user_length,
                             const uint8_t *public_blob, size_t blob_length);
/* Available only with the versioned FrankenTerm libssh2 evidence extension.
 * An unpatched backend remains usable for the other explicit auth methods. */
int ftssh_supports_publickey_interactive(void);
/* Explicitly authorize this key followed, if required, by keyboard-interactive.
 * One finite monotonic deadline (1..120000 ms) covers signing and every MFA
 * round. The key may authenticate completely. Otherwise authentication_step
 * returns AUTH_PARTIAL_REQUIRED only for fresh, bounded, actual SIGNED-key
 * partial success that advertises keyboard-interactive. No method inference,
 * unsigned partial success, key enumeration, or password fallback is allowed.
 * The caller must also authorize the second factor for this exact identity. */
int ftssh_begin_publickey_interactive_authentication(ftssh_session *session,
    const char *user, size_t user_length, const uint8_t *public_blob, size_t blob_length,
    uint32_t timeout_milliseconds);
/* Consume AUTH_PARTIAL_REQUIRED once and transfer to the same bounded native
 * worker used by keyboard-interactive. Retains the exact account, approved
 * peer, connection and original deadline. No new none/method probe is sent.
 * A partial handle cannot open any channel or start another auth method. */
int ftssh_continue_interactive_authentication(ftssh_session *session);
/* Select password authentication only after the same exact host approval.
 * No secret is requested or transmitted until authentication_step has checked
 * the server's complete bounded method list and returned PASSWORD_NEEDED.
 * This is the SSH password method, not keyboard-interactive or password change.
 * Success without the selected method and partial authentication are refused.
 */
int ftssh_begin_password_authentication(ftssh_session *session, const char *user, size_t user_length);
/* One complete secret, copied into owned bounded storage, after PASSWORD_NEEDED.
 * Length is 1..FTSSH_PASSWORD_CAPACITY bytes. Resubmission is refused. The copy
 * survives EAGAIN, is cleared on a terminal result/close, and is never returned.
 */
int ftssh_supply_password(ftssh_session *session, const uint8_t *password, size_t length);
/* Select keyboard-interactive only after exact host approval. At most two native
 * workers exist globally, including completed workers not yet joined; each has
 * a 512 KiB stack outside the libssh2 allocation budget. The worker exclusively
 * owns this session and every adopted parent, including nonblocking I/O. Its
 * finite monotonic deadline includes method discovery and every prompt wait.
 * timeout_milliseconds is 1..120000. No key/password fallback or partial-auth
 * continuation is inferred. Success requires at least one answered challenge
 * (an explicitly answered zero-prompt information round counts). A worker
 * allocation failure sends no auth request.
 */
int ftssh_begin_interactive_authentication(ftssh_session *session, const char *user, size_t user_length,
                                         uint32_t timeout_milliseconds);
/* While the worker exists, authentication_step returns AGAIN, INTERACTIVE_NEEDED,
 * or INTERACTIVE_FINISHED. descriptor/directions refer to its notification pipe,
 * never the network socket. All other ordinary access (including stats) returns
 * STATE. A notification is only a wakeup; recheck authentication_step.
 *
 * Challenge encoding uses big-endian u32 length-prefixed UTF-8 title/instructions,
 * then u32 prompt count, then [u8 echo (0/1), length-prefixed prompt] per prompt.
 * Strings contain no NUL. Each published round is numbered 1..ROUND_CAPACITY.
 * The returned bytes are public untrusted server text, never local host identity.
 */
int ftssh_copy_interactive_challenge(ftssh_session *session, uint8_t *output, size_t capacity,
                                   size_t *length, uint32_t *round);
/* Response encoding: big-endian u32 count followed by that many length-prefixed
 * byte strings, each 0..RESPONSE_CAPACITY without NUL. Exact pending round/count
 * and one submission are required. Empty response lists acknowledge information
 * rounds. The copied answers are cleared after callback consumption or failure.
 */
int ftssh_supply_interactive_responses(ftssh_session *session, uint32_t round,
                                     const uint8_t *responses, size_t length);
/* Nonblocking cancellation: wake prompt/socket waits without touching libssh2.
 * Idempotent when no worker exists. close/destroy also request cancellation but
 * return AGAIN while the worker remains owned; no native state is freed then.
 */
int ftssh_cancel_interactive_authentication(ftssh_session *session);
/* Join and release the worker, returning its final authentication result. This
 * may wait: call on an isolated blocking worker, never a cooperative executor or
 * UI thread, after FINISHED or cancellation. No other handle call may overlap.
 * Every normal terminal result restores ordinary exclusive handle ownership.
 * A join failure retains ownership and returns STATE rather than freeing it.
 */
int ftssh_finish_interactive_authentication(ftssh_session *session);
/* Discovers supported methods after host approval, then offers only this key.
 * FTSSH_AUTH_REJECTED means the server explicitly refused the unsigned public
 * key offer before entering the signing callback. Only that result permits an
 * owner to consider another explicitly configured identity on a new connection.
 * This proof requires the versioned backend; stock ambiguity is AUTH_FAILED.
 * FTSSH_AUTH_FAILED is terminal: an ordinary key-only call does not continue
 * partial authentication. Only the explicitly combined API can return
 * AUTH_PARTIAL_REQUIRED with validated backend evidence. Failure NEVER permits key
 * fallback. No publickey method, or success without the selected key's signature,
 * is FTSSH_UNSUPPORTED. Every authentication failure terminates this handle;
 * no shell is opened. For a selected password, PASSWORD_NEEDED requests one
 * foreground response and PASSWORD_FAILED is terminal, never key-retry proof.
 */
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

/* One explicitly approved transit channel, with no session, PTY or command.
 * After forward_step succeeds, forward_adopt atomically transfers the complete
 * outer handle into a fresh inner SSH handle and sets *outer to NULL. Failure
 * leaves the outer handle owned by the caller. The inner handle's callbacks
 * consume ONLY the direct-tcpip channel; the root descriptor supplies readiness,
 * never destination plaintext. Its one serial owner drives and closes the
 * entire chain. Up to FTSSH_MAX_JUMP_HOPS transit sessions are supported;
 * each child still requires separate peer verification and authentication.
 * No transit shell, transport sharing or unbounded nesting is supported.
 */
int ftssh_begin_forward(ftssh_session *session, const char *host, size_t host_length, uint16_t port);
int ftssh_forward_step(ftssh_session *session);
int ftssh_forward_adopt(ftssh_session **outer, size_t max_allocated, ftssh_session **inner);

int ftssh_begin_shell(ftssh_session *session, uint32_t columns, uint32_t rows,
                      uint32_t pixel_width, uint32_t pixel_height);
/* Instead of a shell: one remote command (printable ASCII, at most
 * FTSSH_COMMAND_CAPACITY bytes) with no PTY, so the channel carries its bytes
 * unchanged. Its stderr is discarded. ftssh_shell_step completes the start;
 * read, write, completion and close then work as for a shell; resize does not. */
int ftssh_begin_command(ftssh_session *session, const char *command, size_t length);
int ftssh_shell_step(ftssh_session *session);
/* Admission copies the whole bounded unit or accepts nothing. Success means
 * accepted into this adapter's single 16 KiB slot, NOT remote delivery. */
int ftssh_admit_write(ftssh_session *session, const uint8_t *bytes, size_t length);
/* Retains the exact write pointer and length across libssh2 EAGAIN. A completed
 * write receipt requires ciphertext to drain through every hop into the root
 * socket; accepting it into an encrypted-output queue alone is insufficient. */
int ftssh_flush_write(ftssh_session *session);
int ftssh_read(ftssh_session *session, uint8_t *output, size_t capacity, size_t *length);
/* Only after read returned EOF with all buffered output drained. Make bounded
 * nonblocking progress toward remote channel closure without sending EOF or
 * CLOSE ourselves. OK means observed channel closure; AGAIN waits for readiness;
 * YIELD means the receive quantum was consumed and the owner should yield then
 * retry. On each non-error return, completion contains the evidence available.
 * The owner supplies a finite overall deadline: remote exit status is optional.
 * libssh2 cannot distinguish reported zero from absent status. Both stay UNKNOWN;
 * never turn UNKNOWN into successful process completion. Signal names are bounded
 * printable ASCII. Neither exit evidence nor channel close proves input effects.
 */
int ftssh_completion_step(ftssh_session *session, ftssh_completion *completion);
/* No acknowledged geometry exists in ordinary SSH. Success means the local
 * window-change request completed. Resume the same request after AGAIN. */
int ftssh_resize(ftssh_session *session, uint32_t columns, uint32_t rows,
                  uint32_t pixel_width, uint32_t pixel_height);
/* Liveness probe for a live shell: one keepalive request that asks the server
 * to reply, so a responsive server's bytes advance received_bytes. Call at
 * least 2 s apart. AGAIN means nothing was sent (a write, resize or library
 * packet is pending, the socket buffer is nearly full, or libssh2 held back a
 * probe within 2 s of its last); UNSUPPORTED means
 * the platform cannot report queued socket bytes. Never leaves a half-sent
 * packet that would block later writes: if one remains, the session fails. */
int ftssh_keepalive(ftssh_session *session);
/* Advances one bounded encrypted-output turn. RETRY requests a cooperative
 * yield, then another turn without waiting for new socket bytes (queued output
 * advanced or an ancestor's receive quantum was exhausted). */
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
