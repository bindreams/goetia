/* Daemon for the graceful-stop probe (goetia #263). Throwaway.
 * Usage: sigterm-logger <logfile>
 * Logs "started pid=<pid> t=<unix seconds>", then on every SIGTERM "SIGTERM t=<unix seconds>", and
 * keeps running (it never exits by itself), so the end of the process is launchd's SIGKILL. */
#include <signal.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGTERM);
    sigprocmask(SIG_BLOCK, &set, NULL);
    FILE *f = fopen(argv[1], "a");
    if (!f) return 3;
    setvbuf(f, NULL, _IOLBF, 0);
    fprintf(f, "started pid=%d t=%.3f\n", (int)getpid(), now());
    for (;;) {
        int sig;
        if (sigwait(&set, &sig) == 0) fprintf(f, "SIGTERM t=%.3f\n", now());
    }
}
