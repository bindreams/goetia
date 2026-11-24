/* Claim under test (goetia queue, question 6): XNU posts a process-directed signal (kill(getpid(), sig)) to one thread
 *   chosen at send time, so a signal that another thread could take is never left pending process-wide for a thread
 *   that unblocks it later.
 *
 * Threads: M (main, sender; blocks SIGUSR1), A (the marked thread; blocks it, unblocks it right after the send, then
 * blocks it again), B (variable). Per pass: M sends kill(getpid(), SIGUSR1), tells A to unblock, waits for A to finish
 * its unblock/block and for the handler to have run once; the handler records which thread ran it.
 * Variants of B:
 *   busy    B leaves SIGUSR1 unblocked and spins in user space.
 *   sleep   B leaves it unblocked and sleeps in read(2) on an empty pipe.
 *   none    control: B blocks it too, so only A can take it when it unblocks.
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

static atomic_int go, done, handled, stop;
static atomic_uintptr_t last;
static int pipefd[2];
static int variant_none, variant_sleep;

/* A failure bound surfaced to a human: the handler is expected on every pass; this only ends a hang. */
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

static void *thread_a(void *arg) {
    (void)arg;
    sigmask_op(SIG_BLOCK);
    int seen = 0;
    while (!atomic_load(&stop)) {
        int g = atomic_load(&go);
        if (g == seen) continue;
        sigmask_op(SIG_UNBLOCK);
        sigmask_op(SIG_BLOCK);
        seen = g;
        atomic_store(&done, g);
    }
    return NULL;
}

static void *thread_b(void *arg) {
    (void)arg;
    if (variant_none) {
        sigmask_op(SIG_BLOCK);
    } else {
        sigmask_op(SIG_UNBLOCK);
    }
    char c;
    while (!atomic_load(&stop)) {
        if (variant_sleep) {
            ssize_t n = read(pipefd[0], &c, 1); /* returns EINTR after a handler ran here; EOF once stop closes the pipe */
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

    sigmask_op(SIG_BLOCK); /* M; A and B inherit it, B undoes it for itself unless this is the control */
    pthread_t ta, tb;
    pthread_create(&ta, NULL, thread_a, NULL);
    pthread_create(&tb, NULL, thread_b, NULL);
    /* Handles for naming the thread that ran the handler (pthread_t is a pointer on macOS). */
    uintptr_t A = (uintptr_t)ta, B = (uintptr_t)tb, M = (uintptr_t)pthread_self();

    long on_a = 0, on_b = 0, on_m = 0, on_other = 0, miscount = 0;
    for (int i = 1; i <= passes; i++) {
        int before = atomic_load(&handled);
        kill(getpid(), SIGUSR1);
        atomic_store(&go, i);
        double t0 = now();
        while (atomic_load(&done) != i || atomic_load(&handled) == before) {
            if (now() - t0 > BOUND_S) {
                printf("FAILURE BOUND: pass %d: done=%d handled=%d before=%d after %d s; stopping\n", i, atomic_load(&done), atomic_load(&handled),
                       before, BOUND_S);
                goto out;
            }
        }
        if (atomic_load(&handled) != before + 1) miscount++;
        uintptr_t who = atomic_load(&last);
        if (who == A) on_a++;
        else if (who == B) on_b++;
        else if (who == M) on_m++;
        else on_other++;
    }
out:
    atomic_store(&stop, 1);
    close(pipefd[1]); /* ends B's read */
    pthread_join(ta, NULL);
    /* A busy or sleeping B ends on `stop`; a sleeping B needs the pipe closed above. */
    pthread_join(tb, NULL);
    printf("variant=%s passes=%d: handler ran on A (the marked thread, which unblocked after the send): %ld; on B: %ld; on the sender M: %ld; "
           "elsewhere: %ld; passes where the handler count was not exactly one more: %ld\n",
           argv[1], passes, on_a, on_b, on_m, on_other, miscount);
    return 0;
}
