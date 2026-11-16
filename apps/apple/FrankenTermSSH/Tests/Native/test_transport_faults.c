/* Test-only syscall fault injection around the real production SSH driver.
 *
 * Build from the FrankenTermSSH package directory:
 * cc -std=c11 -D_DEFAULT_SOURCE -Wall -Wextra -Werror -fPIC -shared \
 *   -I Sources/CFrankenTermSSH/include Tests/Native/test_transport_faults.c \
 *   -lssh2 -pthread -o /tmp/libfrankenterm-ssh-faults.so
 * python3 Tests/Native/test_native.py /tmp/libfrankenterm-ssh-faults.so
 *
 * libssh2, key exchange, encryption, authentication and the independent libssh
 * server remain real. Only an individual POSIX send/recv result is replaced.
 * This translation unit is never linked into an application product.
 */
#include <errno.h>
#include <stdatomic.h>
#include <sys/socket.h>

static _Atomic int send_error, receive_error;
static _Atomic unsigned int send_failures, receive_failures;
static ssize_t fault_send(int fd, const void *bytes, size_t length, int flags);
static ssize_t fault_receive(int fd, void *bytes, size_t length, int flags);

#define send fault_send
#define recv fault_receive
#include "../../Sources/CFrankenTermSSH/FrankenTermSSH.c"
#undef send
#undef recv

static int consume_fault(_Atomic unsigned int *remaining, _Atomic int *error) {
    unsigned int count = atomic_load(remaining);
    while(count) {
        if(atomic_compare_exchange_weak(remaining, &count, count - 1))
            return atomic_load(error);
    }
    return 0;
}

static ssize_t fault_send(int fd, const void *bytes, size_t length, int flags) {
    int injected = consume_fault(&send_failures, &send_error);
    if(injected) { errno = injected; return -1; }
    return send(fd, bytes, length, flags);
}

static ssize_t fault_receive(int fd, void *bytes, size_t length, int flags) {
    int injected = consume_fault(&receive_failures, &receive_error);
    if(injected) { errno = injected; return -1; }
    return recv(fd, bytes, length, flags);
}

void ftssh_test_set_transport_faults(int send_errno, unsigned int send_count,
                                    int receive_errno, unsigned int receive_count) {
    atomic_store(&send_error, send_errno);
    atomic_store(&receive_error, receive_errno);
    atomic_store(&send_failures, send_count);
    atomic_store(&receive_failures, receive_count);
}

size_t ftssh_test_queued_ciphertext(ftssh_session *session) {
    size_t bytes = 0;
    for(; session; session = session->parent) bytes += session->egress_count;
    return bytes;
}
