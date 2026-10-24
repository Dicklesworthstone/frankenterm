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
    uint32_t rows;              /* offset 12; 1..512; 2*rows*columns<=65536 */
    uint32_t pixel_width;       /* offset 16; 0..65535 */
    uint32_t pixel_height;      /* offset 20; 0..65535 */
    uint32_t scrollback_rows;   /* offset 24; 0..4096; the most history kept:
                                * a grid keeps the rows that fit beside both
                                * screens, (65536-2*rows*columns)/columns */
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
/* Replaces the configured base palette: the machine's theme. Exactly
 * FT_TERMINAL_BASE_PALETTE_BYTES of sRGB R,G,B,A bytes for 256 indexed colors,
 * then foreground, background, cursor foreground, cursor background, cursor
 * border, selection foreground, selection background. A larger length is
 * CAPACITY, any other is INVALID. Terminal OSC color changes remain transient
 * overrides on top; their resets and color queries see this palette. Advances
 * the frame palette revision. */
#define FT_TERMINAL_BASE_PALETTE_BYTES 1052u
uint32_t ft_terminal_v1_set_base_palette(uint64_t session, const uint8_t *rgba, uint32_t length);

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
 * Header flag 32 assigns bytes 192..256 as a 512-bit soft-wrap bitmap:
 * captured row N uses bit N%8 of byte 192+N/8. Unused bits are zero; without
 * the flag the entire bitmap is zero. A set bit is canonical continuation,
 * not permission to join missing rows or different source/geometry captures.
 * Existing row/cell table sizes are unchanged. Reject unknown header flags.
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

/* Mux client: the protocol of a FrankenTerm mux server (codec v46 and v47,
 * the fleet's v0.13 servers, and the current window 61..70, a FrankenTerm
 * GUI's own mux) as a byte-in/byte-out state machine. The caller
 * owns the channel (an SSH command running `nc -N -U <socket>`), the
 * terminals the output is drawn on, and all scheduling; nothing here does
 * I/O or keeps a pointer. Status values are the FT_TERMINAL_* codes.
 *
 * After create, send what ft_mux_v1_outgoing returns, pass everything the
 * channel delivers to ft_mux_v1_receive, and drain ft_mux_v1_next_event
 * after each call until it reports FT_MUX_EVENT_NONE. An event's payload
 * (length bytes) stays readable through ft_mux_v1_event_copy until the
 * next ft_mux_v1_next_event call.
 *
 *   READY      payload: the server's version string; the pane list follows
 *   PANES      payload: JSON array of {pane_id, tab_id, window_id, tab_title,
 *              title, workspace, working_dir, size{rows, cols, pixel_width,
 *              pixel_height, dpi}, is_active, is_zoomed, top_row, left_col,
 *              physical_top}; again whenever the desktop adds or reshapes
 *              a tab, or a pane closes
 *   OUTPUT     pane_id, rows, cols; payload: bytes for that pane's terminal,
 *              which must be rows x cols before they are fed
 *   PANE_TITLE pane_id; payload: the title
 *   PANE_GONE  pane_id
 *   FAILED     payload: why; the session is over, close the channel
 *   SEARCH_RESULTS pane_id; payload: JSON {search, matches: [{row,
 *              start_column, end_row, end_column, text, text_start,
 *              text_end}], searched_rows, error}: the newest matches,
 *              newest first; text is the matched rows (at most four,
 *              newline-joined) and text_start..text_end the match within
 *              it, as UTF-8 byte offsets
 *   CONTEXT    pane_id; payload: JSON {request, lines: [{row, text}]}
 *   PHONE_SIZE pane_id, rows, cols: the phone's size is in effect for the
 *              pane; payload: JSON {rows, cols, pixel_width, pixel_height,
 *              dpi}, the desktop's size to give back. An empty payload (rows
 *              and cols 0): it no longer is. Keep the latest per pane across
 *              connections for ft_mux_v1_adopt_phone_size.
 *   MOUSE_GRAB pane_id; payload: one byte, 1 when the pane's program has
 *              asked for the mouse (a tap is then a click), 0 when not
 */
#define FT_MUX_EVENT_NONE 0u
#define FT_MUX_EVENT_READY 1u
#define FT_MUX_EVENT_PANES 2u
#define FT_MUX_EVENT_OUTPUT 3u
#define FT_MUX_EVENT_PANE_TITLE 4u
#define FT_MUX_EVENT_PANE_GONE 5u
#define FT_MUX_EVENT_FAILED 6u
#define FT_MUX_EVENT_SEARCH_RESULTS 7u
#define FT_MUX_EVENT_CONTEXT 8u
#define FT_MUX_EVENT_PHONE_SIZE 9u
#define FT_MUX_EVENT_MOUSE_GRAB 10u
#define FT_MUX_SEARCH_CASE_SENSITIVE 1u
#define FT_MUX_SEARCH_REGEX 2u
#define FT_MUX_MAX_RECEIVE_BYTES 1048576u
#define FT_MUX_MAX_WRITE_BYTES 65536u
#define FT_MUX_MAX_COPY_BYTES 16777216u

uint32_t ft_mux_v1_create(uint64_t *session);
uint32_t ft_mux_v1_release(uint64_t session);
uint32_t ft_mux_v1_receive(uint64_t session, const uint8_t *bytes, uint32_t length);
/* The channel closed; a FAILED event follows. */
uint32_t ft_mux_v1_closed(uint64_t session);
/* Copies up to capacity of the bytes to send next; copied bytes are taken. */
uint32_t ft_mux_v1_outgoing(uint64_t session, uint8_t *output, uint32_t capacity, uint32_t *written);
uint32_t ft_mux_v1_next_event(uint64_t session, uint32_t *kind, uint64_t *pane_id,
                              uint32_t *rows, uint32_t *cols, uint32_t *length);
uint32_t ft_mux_v1_event_copy(uint64_t session, uint32_t offset, uint8_t *output,
                              uint32_t capacity, uint32_t *written);
/* Asks for the pane list again; a PANES event answers. */
uint32_t ft_mux_v1_refresh_panes(uint64_t session);
/* Draws the pane from scratch (clearing its terminal), then every change. */
uint32_t ft_mux_v1_show(uint64_t session, uint64_t pane_id);
/* Stops drawing the pane. */
uint32_t ft_mux_v1_hide(uint64_t session, uint64_t pane_id);
/* What the pane's terminal typed. Cursor, editing and function key
 * sequences go to the server as key presses, encoded in the modes the
 * pane's program set; a bracketed paste (OUTPUT turns bracketed paste on in
 * the phone's terminal) goes as one paste; everything else as is. A paste
 * may span writes and waits on its own pane for its end. The rest of a
 * paste for a pane that closed ends the session with FAILED, unsent. */
uint32_t ft_mux_v1_write(uint64_t session, uint64_t pane_id, const uint8_t *bytes, uint32_t length);
/* UTF-8 text the server pastes, bracketed when the program asked for it.
 * While a paste written to the pane is unfinished this ends the session
 * with FAILED instead of jumping ahead of it. */
uint32_t ft_mux_v1_paste(uint64_t session, uint64_t pane_id, const uint8_t *text, uint32_t length);
/* Resizes the remote pane (1..512 each), remembering the size the desktop
 * gave it; restore_size puts that size back. */
uint32_t ft_mux_v1_resize(uint64_t session, uint64_t pane_id, uint32_t rows, uint32_t cols);
uint32_t ft_mux_v1_restore_size(uint64_t session, uint64_t pane_id);
/* After the pane list arrives: an earlier connection left the pane at the
 * phone's size rows x cols (1..512 each) without giving back the desktop's
 * (desktop rows and cols 1..4096), as its last PHONE_SIZE event said. If the
 * pane is still listed at exactly that size, restore_size will give it the
 * desktop's size; *adopted is 1. Otherwise (the desktop resized it since, or
 * the id now names another pane) nothing changes and *adopted is 0. */
uint32_t ft_mux_v1_adopt_phone_size(uint64_t session, uint64_t pane_id, uint32_t desktop_rows,
                                    uint32_t desktop_cols, uint32_t desktop_pixel_width,
                                    uint32_t desktop_pixel_height, uint32_t desktop_dpi,
                                    uint32_t rows, uint32_t cols, uint32_t *adopted);
/* Turns a mouse wheel steps notches (nonzero; negative toward older output,
 * at most 64 sent per call) over the pane's screen cell column,row (each at
 * most 4096). The server reports them to a program that asked for the
 * mouse and sends a full-screen program arrow keys, as for the desktop's
 * wheel; other programs ignore them. */
uint32_t ft_mux_v1_wheel(uint64_t session, uint64_t pane_id, int32_t steps, uint32_t column, uint32_t row);
/* Clicks the left button (press, release) on the pane's screen cell
 * column,row (each at most 4096); the server reports it to a program that
 * asked for the mouse. */
uint32_t ft_mux_v1_click(uint64_t session, uint64_t pane_id, uint32_t column, uint32_t row);
/* Searches the pane's scrollback on the server, newest rows first, for
 * UTF-8 text (at most 4096 bytes) with FT_MUX_SEARCH_* flags; a
 * SEARCH_RESULTS event with this id answers (with an error for a pane that
 * has closed). CLOSED before the handshake. */
uint32_t ft_mux_v1_search(uint64_t session, uint64_t pane_id, const uint8_t *text, uint32_t length,
                          uint32_t flags, uint64_t *search);
/* The text of the pane's rows first_row..first_row+count (count at most
 * 1000); a CONTEXT event with this id answers. */
uint32_t ft_mux_v1_context(uint64_t session, uint64_t pane_id, int64_t first_row, uint32_t count,
                           uint64_t *request);

#ifdef __cplusplus
}
#endif
#endif
