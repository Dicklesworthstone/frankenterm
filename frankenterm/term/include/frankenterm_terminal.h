#ifndef FRANKENTERM_TERMINAL_H
#define FRANKENTERM_TERMINAL_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define FT_TERMINAL_ABI_VERSION 1u
#define FT_TERMINAL_OK 0u
#define FT_TERMINAL_INVALID 1u
#define FT_TERMINAL_STALE 2u
#define FT_TERMINAL_CLOSED 3u
#define FT_TERMINAL_CAPACITY 4u
#define FT_TERMINAL_BUSY 5u
#define FT_TERMINAL_UNSUPPORTED 6u
#define FT_TERMINAL_PANIC 7u
#define FT_TERMINAL_SEQUENCE_EXHAUSTED 8u
#define FT_TERMINAL_VIEWPORT_UNAVAILABLE 9u

#define FT_TERMINAL_MAX_FEED_BYTES 16384u
#define FT_TERMINAL_MAX_INPUT_BYTES 65536u
#define FT_TERMINAL_MAX_TEXT_BYTES 16384u
#define FT_TERMINAL_MAX_REPLY_BYTES 16384u
#define FT_TERMINAL_MAX_FRAME_BYTES 8388608u
#define FT_TERMINAL_CURRENT_VIEWPORT INT64_MIN

/* Native-endian C fields; UUID arrays contain the 16 RFC 4122 wire bytes.
 * No padding or reserved field is an extension channel. Version 1 requires
 * sizeof(config)==80, struct_size==80 and reserved==0. An incompatible
 * version/size is refused before a terminal is created.
 */
typedef struct ft_terminal_v1_config {
    uint32_t abi_version;       /* offset 0 */
    uint32_t struct_size;       /* offset 4 */
    uint32_t columns;           /* offset 8; 1..512 */
    uint32_t rows;              /* offset 12; 1..512; rows*columns<=65536 */
    uint32_t pixel_width;       /* offset 16; 0..65535 */
    uint32_t pixel_height;      /* offset 20; 0..65535 */
    uint32_t scrollback_rows;   /* offset 24; 0..4096;
                                * (2*rows+scrollback_rows)*columns<=65536 */
    uint32_t reserved;          /* offset 28; must be zero */
    uint8_t channel_id[16];     /* offset 32; nonzero, direct SSH only */
    uint8_t transport_id[16];   /* offset 48; nonzero */
    uint64_t route_revision;   /* offset 64; nonzero */
    uint64_t font_revision;    /* offset 72; nonzero */
} ft_terminal_v1_config;

#if defined(__cplusplus)
static_assert(sizeof(ft_terminal_v1_config) == 80, "terminal config ABI size");
static_assert(offsetof(ft_terminal_v1_config, channel_id) == 32, "terminal UUID ABI offset");
static_assert(offsetof(ft_terminal_v1_config, route_revision) == 64, "terminal revision ABI offset");
#elif defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(ft_terminal_v1_config) == 80, "terminal config ABI size");
_Static_assert(offsetof(ft_terminal_v1_config, channel_id) == 32, "terminal UUID ABI offset");
_Static_assert(offsetof(ft_terminal_v1_config, route_revision) == 64, "terminal revision ABI offset");
#endif

/* Ownership and threading contract (all calls):
 * - Handles are typed, generation-fenced integers, never pointers. At most 16
 *   sessions and one acquired immutable frame per session are retained.
 * - Calls are synchronous and never perform transport I/O or spawn a task.
 *   A short registry mutex serializes native calls; there are no callbacks.
 *   Callers serialize each owner across downstream asynchronous admission.
 *   BUSY denotes an outstanding reply/frame, not unrelated owner contention.
 * - All pointers must be valid/aligned for the indicated size for the entire
 *   call. Mutable buffers and scalar outputs must not overlap each other or
 *   input/config memory. No pointer or borrowed byte is retained after return.
 *   Byte buffers contain initialized storage; scalar outputs may be uninitialized.
 *   A byte pointer may be NULL only when its length/capacity is zero. Scalar
 *   outputs are mandatory. Rust allocations are released only by these APIs.
 * - Outputs are meaningful only for OK. CAPACITY never publishes a partial
 *   encoded input, paste, or reply. feed/resize may have changed model state
 *   before a resource/semantic fault; a failed mutating call must not be
 *   retried as though it consumed nothing. Close that session instead.
 * - A recovered Rust panic returns PANIC and permanently poisons the session.
 *   No panic unwinds into C. The artifact must retain panic=unwind to execute
 *   recovery; native-ffi refuses to compile with panic=abort.
 *   The first call wraps the existing host panic hook once to suppress only
 *   canonical recoverable panics. Unmarked fatal/EPIPE policy is preserved.
 *   Later host hooks must preserve that chain and respect the recovery marker.
 * - This boundary never means that bytes reached or executed at a remote PTY.
 */
uint32_t ft_terminal_v1_create(const ft_terminal_v1_config *config, uint64_t *session);
/* Close is idempotent and releases queued replies. A copied reply must be
 * settled first or close returns BUSY. release is unconditional cleanup: it
 * invalidates the session, frame and any orphaned reply lease; it never replays
 * bytes already admitted to an independent downstream owner. A late reply
 * settle or stale/double release is rejected by the generation fence. */
uint32_t ft_terminal_v1_close(uint64_t session);
uint32_t ft_terminal_v1_release(uint64_t session);
/* In addition to the byte cap, each feed has a finite expanded-command work
 * budget. Resource refusal (including a reply that cannot be retained whole)
 * returns CAPACITY and permanently closes this model. No truncated command
 * execution is exposed as a successful frame. This is distinct from refusing
 * an oversized input slice before any work; callers close after either error.
 */
uint32_t ft_terminal_v1_feed(uint64_t session, const uint8_t *bytes, uint32_t length);
uint32_t ft_terminal_v1_resize(uint64_t session, uint32_t columns, uint32_t rows,
                              uint32_t pixel_width, uint32_t pixel_height);
uint32_t ft_terminal_v1_set_font_revision(uint64_t session, uint64_t revision);

/* Pure mode-aware encoders. No terminal writer, echo, focus, or input state is
 * changed. Caller must serialize mode changes with admission of these bytes.
 * HID usage: keyboard page 0x07, 1..0xe7. Modifiers: shift=1, control=2, alt=4,
 * command=8, capsLock=16. Characters are layout-resolved UTF-8, at most 256
 * bytes. Phase: down=0, repeated=1, up=2, complete press=3. Repeated uses the
 * existing canonical down encoding; unsupported HID usages are refused.
 * Text is at most MAX_TEXT_BYTES and uses canonical Enter/newline semantics;
 * CRLF is one Enter, each committed scalar is a complete press. */
uint32_t ft_terminal_v1_encode_key(uint64_t session, uint32_t usage, uint32_t modifiers,
                                  uint32_t phase, const uint8_t *characters,
                                  uint32_t characters_length, uint8_t *output,
                                  uint32_t capacity, uint32_t *written);
uint32_t ft_terminal_v1_encode_text(uint64_t session, const uint8_t *text, uint32_t length,
                                   uint8_t *output, uint32_t capacity, uint32_t *written);
/* No bracket delimiters are returned. flags bit0 is negotiated bracketed
 * paste; all other bits are zero. The Swift egress owner frames and settles
 * the paste. ESC/C1 and non-text controls are refused before copying. */
uint32_t ft_terminal_v1_prepare_paste(uint64_t session, const uint8_t *text, uint32_t length,
                                     uint8_t *output, uint32_t capacity,
                                     uint32_t *written, uint32_t *flags);

/* Copy exactly one complete reply, retaining its reservation in Rust. Empty
 * queue is OK with length=0, sequence=0. After coordinator admission or a
 * deliberate whole drop, settle that exact nonzero sequence before another
 * copy or cancellation/close. Disposition: admitted=0, droppedWhole=1. A stale
 * or repeated settle cannot release a successor. Neither function routes to
 * the focused pane; use only this session's immutable source identity. */
uint32_t ft_terminal_v1_reply_copy(uint64_t session, uint8_t *output, uint32_t capacity,
                                  uint32_t *length, uint64_t *sequence);
uint32_t ft_terminal_v1_reply_settle(uint64_t session, uint64_t sequence, uint32_t disposition);

/* Capture the canonical full-replacement FTSF v1 wire transaction. Its bytes
 * and identity/revision witnesses are immutable until frame_release or parent
 * session release. feed/resize/font changes never mutate an acquired frame.
 * Acquire returns BUSY while a frame is owned. span copies check the complete
 * range before writing; capacity may be up to MAX_FRAME_BYTES. An explicitly
 * selected history viewport that was evicted returns VIEWPORT_UNAVAILABLE;
 * this is not a terminal failure. Feeding/replies and the live viewport still
 * work. Do not present the prior rows as current or reuse their input geometry. */
uint32_t ft_terminal_v1_frame_acquire(uint64_t session, int64_t viewport_top,
                                     uint64_t *frame, uint32_t *length);
uint32_t ft_terminal_v1_frame_copy(uint64_t frame, uint32_t offset, uint8_t *output,
                                  uint32_t capacity, uint32_t *written);
uint32_t ft_terminal_v1_frame_release(uint64_t frame);

#ifdef __cplusplus
}
#endif
#endif
