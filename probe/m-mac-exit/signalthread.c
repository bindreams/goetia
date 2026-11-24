/* Claim under test (goetia queue, question 6): XNU posts a process-directed signal (kill(getpid(), sig)) to one thread
 *   chosen at send time, so a signal that another thread could take is never left pending process-wide for the marked
 *   thread to take as it unblocks.
 *
 * Threads: A (the main thread, "marked": blocks SIGUSR1, sends kill(getpid(), SIGUSR1) to the process, then unblocks it
 * at once, in the same thread, so nothing separates the send from the unblock) and B. Per pass the handler records which
 * thread ran it. Variants of B:
 *   busy   B leaves SIGUSR1 unblocked and spins in user space.
 *   sleep  B leaves it unblocked and sleeps in read(2) on an empty pipe.
 *   none   control: B blocks it too, so no thread can take it at the send. A unblocks first; if the handler did not run
 *          there (the unblock syscall returns after any signal it can take has run), B is asked to unblock.
 * For busy and sleep a pass starts only once B is outside its handler (its loop has seen the last handler run), so the
 * signal is never sent while B has it masked by being inside the handler.
 *
 * Build: cc -O1 -Wall -pthread -o signalthread signalthread.c
 * Run:   ./signalthread <busy|sleep|none> <passes>
 */
#define _DARWIN_C_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static atomic_int handled, b_ack, b_ready, b_turn, b_turn_done, stop;
static atomic_uintptr_t last;
static int pipefd[2];
static int variant_none, variant_sleep;

/* A failure bound surfaced to a human: a handler is expected on every pass of busy and sleep; this only ends a hang. */
#define BOUND_S 30

static void handler(int sig) {
    (void)sig;
    atomic_store(&last, (uintptr_t)pthread_self());
    atomic_fetch_add(&handled, 1);
}

static void sigmask_op(int how) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    pthread_sigmask(how, &s, NULL);
}

static void *thread_b(void *arg) {
    (void)arg;
    if (!variant_none) sigmask_op(SIG_UNBLOCK);
    atomic_store(&b_ready, 1);
    int turn_seen = 0;
    while (!atomic_load(&stop)) {
        if (variant_none) {
            int t = atomic_load(&b_turn);
            if (t != turn_seen) {
                sigmask_op(SIG_UNBLOCK);
                sigmask_op(SIG_BLOCK);
                turn_seen = t;
                atomic_store(&b_turn_done, t);
            }
            continue;
        }
        atomic_store(&b_ack, atomic_load(&handled));
        if (variant_sleep) {
            char c;
            ssize_t n = read(pipefd[0], &c, 1); /* EINTR after a handler ran here; EOF once the pipe is closed at the end */
            if (n == 0) break;
        }
    }
    return NULL;
}

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc != 3) {
        fprintf(stderr, "usage: see the top of the file\n");
        return 2;
    }
    variant_none = !strcmp(argv[1], "none");
    variant_sleep = !strcmp(argv[1], "sleep");
    int passes = atoi(argv[2]);
    if (pipe(pipefd)) return 2;

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = handler; /* no SA_RESTART: a sleeping B's read returns EINTR */
    sigemptyset(&sa.sa_mask);
    sigaction(SIGUSR1, &sa, NULL);

    sigmask_op(SIG_BLOCK); /* A; B inherits it, and unblocks it for itself unless this is the control */
    pthread_t tb;
    pthread_create(&tb, NULL, thread_b, NULL);
    uintptr_t A = (uintptr_t)pthread_self(), B = (uintptr_t)tb;
    while (!atomic_load(&b_ready)) {} /* B's mask is settled */

    long on_a = 0, on_b = 0, on_neither = 0;
    long a_during_unblock = 0;
    int last_on_b = 1;
    for (int i = 1; i <= passes; i++) {
        if (!variant_none && last_on_b) {
            double t0 = now();
            while (atomic_load(&b_ack) != atomic_load(&handled)) {
                if (now() - t0 > BOUND_S) {
                    printf("FAILURE BOUND: pass %d: B never left its handler after %d s; stopping\n", i, BOUND_S);
                    goto out;
                }
            }
        }
        int before = atomic_load(&handled);
        kill(getpid(), SIGUSR1); /* A has it blocked; B has it blocked only in the control */
        sigmask_op(SIG_UNBLOCK);
        int ran_here = atomic_load(&handled) != before && atomic_load(&last) == A;
        sigmask_op(SIG_BLOCK);
        if (ran_here) a_during_unblock++;
        if (variant_none && !ran_here) {
            int turn = atomic_load(&b_turn) + 1;
            atomic_store(&b_turn, turn);
            while (atomic_load(&b_turn_done) != turn) {} /* B has returned from its unblock; a signal it can take has run */
            if (atomic_load(&handled) == before) {
                printf("pass %d: neither A nor B ran the handler on unblocking; the signal is still pending somewhere; stopping\n", i);
                on_neither++;
                goto out;
            }
        }
        double t0 = now();
        while (atomic_load(&handled) == before) {
            if (now() - t0 > BOUND_S) {
                printf("FAILURE BOUND: pass %d: no thread ran the handler after %d s; stopping\n", i, BOUND_S);
                on_neither++;
                goto out;
            }
        }
        uintptr_t who = atomic_load(&last);
        last_on_b = (who == B);
        if (who == A) on_a++;
        else if (who == B) on_b++;
        else on_neither++;
    }
out:
    atomic_store(&stop, 1);
    close(pipefd[1]); /* ends a sleeping B's read */
    pthread_join(tb, NULL);
    printf("variant=%s passes=%d: handler ran on A (the marked thread, which unblocked right after the send): %ld (inside the unblock call: %ld); "
           "on B: %ld; on no known thread / not at all: %ld\n",
           argv[1], passes, on_a, a_during_unblock, on_b, on_neither);
    return 0;
}
