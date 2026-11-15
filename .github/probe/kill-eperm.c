/* Probe C: does a signal-0 to an exiting, unreaped child answer EPERM on macOS?
 *
 * For each pass: fork a child (mode 0: exits after a random sleep; mode 1: execs /usr/bin/true;
 * mode 2: execs /bin/launchctl print system). Read its audit-token identity the way cosca does,
 * then send signal 0 by token in a tight loop, plus kill(pid, 0) beside it, until the answer says
 * the child is a zombie or gone. Every errno is tallied; the first EPERM of a pass records the
 * child's pbi_status (SIDL=1 SRUN=2 SSLEEP=3 SSTOP=4 SZOMB=5) and the run of answers before it.
 *
 * Build: cc -O1 -Wall -o kill-eperm kill-eperm.c
 * Run:   ./kill-eperm <passes-mode0> <passes-mode1> <passes-mode2>
 */
#include <errno.h>
#include <fcntl.h>
#include <libproc.h>
#include <mach/message.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/proc_info.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PROC_PIDUNIQIDENTIFIERINFO 17
struct uniqid {
    uint8_t uuid[16];
    uint64_t p_uniqueid, p_puniqueid;
    int32_t p_idversion, p_orig_ppidversion;
    uint64_t r2, r3;
};

static audit_token_t make_token(pid_t pid, uint32_t v) {
    audit_token_t t;
    memset(&t, 0, sizeof t);
    t.val[5] = (unsigned)pid;
    t.val[7] = v;
    return t;
}

static int read_uniq(pid_t p, struct uniqid *u) {
    memset(u, 0, sizeof *u);
    int n = proc_pidinfo(p, PROC_PIDUNIQIDENTIFIERINFO, 0, u, sizeof *u);
    return n == (int)sizeof *u ? 0 : (errno ? errno : -1);
}

static int read_status(pid_t p) {
    struct proc_bsdinfo b;
    int n = proc_pidinfo(p, PROC_PIDTBSDINFO, 0, &b, sizeof b);
    return n == (int)sizeof b ? (int)b.pbi_status : -1;
}

#define MAXERR 140
struct tally {
    long passes;
    long token[MAXERR];  /* answers of proc_signal_with_audittoken(.., 0) */
    long killz[MAXERR];  /* answers of kill(pid, 0): errno, 0 on success */
    long zombie_token[MAXERR], zombie_kill[MAXERR]; /* the answers once pbi_status read SZOMB */
    long eperm_passes;
    long eperm_status[8];
    long ended_zombie, ended_gone, never_identified, kill_eperm_passes;
    int samples, guard_samples;
    long guard_hits, esrch_zombie;
};

static void bump(long *a, int e) { a[(e >= 0 && e < MAXERR) ? e : MAXERR - 1]++; }

static void pass(int mode, struct tally *t) {
    pid_t p = fork();
    if (p == 0) {
        if (mode == 0) {
            usleep(rand() % 3000);
            _exit(0);
        }
        if (mode == 1) execl("/usr/bin/true", "true", (char *)0);
        if (mode >= 2) {
            int dn = open("/dev/null", O_WRONLY);
            dup2(dn, 1);
            dup2(dn, 2);
            execl("/bin/launchctl", "launchctl", "print", "system", (char *)0);
        }
        _exit(127);
    }
    t->passes++;
    struct uniqid u;
    if (read_uniq(p, &u) != 0) {
        t->never_identified++;
        waitpid(p, 0, 0);
        return;
    }
    uint64_t unique = u.p_uniqueid;
    uint32_t ver = (uint32_t)u.p_idversion;
    if (mode == 3) usleep(rand() % 60000);
    int saw_eperm = 0, saw_kill_eperm = 0;
    char run[256];
    size_t rl = 0;
    int last = -2;
    long iters = 0;
    run[0] = 0;
    for (;;) {
        audit_token_t tok = make_token(p, ver);
        /* Signal 0 is EINVAL to the token call: SIGCONT stands in for it (harmless to a running
         * child). Mode 3 sends SIGKILL, the signal cosca sends, after a random delay. */
        int e = proc_signal_with_audittoken(&tok, mode == 3 ? SIGKILL : SIGCONT);
        int k = kill(p, 0) == 0 ? 0 : errno;
        int st0 = read_status(p); /* read after the answers: the child may have moved on since */
        bump(t->token, e);
        if (st0 == 5) { /* SZOMB: what do the sends answer a zombie? */
            bump(t->zombie_token, e);
            bump(t->zombie_kill, k);
            t->ended_zombie++;
            if (e == EPERM) saw_eperm = 1;
            break;
        }
        if (st0 < 0 && e != 0 && e != EPERM && e != ESRCH) break;
        if (++iters > 100000) { /* probe guard */
            t->guard_hits++;
            if (t->guard_samples++ < 5) printf("  GUARD (mode %d): st=%d e=%d k=%d answers: %s\n", mode, st0, e, k, run);
            break;
        }
        bump(t->killz, k);
        if (e != last && rl + 12 < sizeof run) {
            rl += (size_t)snprintf(run + rl, sizeof run - rl, "%d,", e);
            last = e;
        }
        if (k == EPERM) saw_kill_eperm = 1;
        if (e == EPERM && !saw_eperm) {
            saw_eperm = 1;
            int st = read_status(p);
            t->eperm_status[(st >= 0 && st < 7) ? st : 7]++;
            if (t->samples < 12) {
                t->samples++;
                printf("  EPERM sample (mode %d): pbi_status=%d answers so far: %s\n", mode, st, run);
            }
        }
        if (e == 0) continue;
        if (e == ESRCH || e == EPERM) {
            struct uniqid now;
            int r = read_uniq(p, &now);
            if (r == 0 && now.p_uniqueid == unique) {
                if ((uint32_t)now.p_idversion != ver) { ver = (uint32_t)now.p_idversion; continue; }
                /* same unique id, same version, and the send was refused: ESRCH is a zombie. */
                if (e == ESRCH) { t->esrch_zombie++; break; }
                continue; /* EPERM: keep asking until it resolves */
            }
            t->ended_gone++;
            break;
        }
        break;
    }
    if (saw_eperm) t->eperm_passes++;
    if (saw_kill_eperm) t->kill_eperm_passes++;
    waitpid(p, 0, 0);
}

static void report(int mode, struct tally *t) {
    printf("mode %d: passes=%ld never_identified=%ld ended_zombie=%ld esrch_zombie=%ld ended_gone=%ld guard_hits=%ld\n",
           mode, t->passes, t->never_identified, t->ended_zombie, t->esrch_zombie, t->ended_gone, t->guard_hits);
    for (int e = 0; e < MAXERR; e++) {
        if (t->token[e]) printf("  token-signal answer %3d (%s): %ld\n", e, e ? strerror(e) : "ok", t->token[e]);
    }
    for (int e = 0; e < MAXERR; e++) {
        if (t->killz[e]) printf("  kill(pid,0) answer  %3d (%s): %ld\n", e, e ? strerror(e) : "ok", t->killz[e]);
    }
    for (int e = 0; e < MAXERR; e++) {
        if (t->zombie_token[e]) printf("  to a zombie: token-signal answer %3d (%s): %ld\n", e, e ? strerror(e) : "ok", t->zombie_token[e]);
        if (t->zombie_kill[e]) printf("  to a zombie: kill(pid,0) answer  %3d (%s): %ld\n", e, e ? strerror(e) : "ok", t->zombie_kill[e]);
    }
    printf("  passes with a token-signal EPERM: %ld; passes with a kill() EPERM: %ld\n", t->eperm_passes,
           t->kill_eperm_passes);
    printf("  pbi_status at first EPERM: idl=%ld run=%ld sleep=%ld stop=%ld zomb=%ld other=%ld\n",
           t->eperm_status[1], t->eperm_status[2], t->eperm_status[3], t->eperm_status[4], t->eperm_status[5],
           t->eperm_status[0] + t->eperm_status[6] + t->eperm_status[7]);
    fflush(stdout);
}

int main(int argc, char **argv) {
    printf("uid=%d\n", (int)getuid());
    srand((unsigned)time(0));
    for (int mode = 0; mode < 4; mode++) {
        long n = argc > mode + 1 ? atol(argv[mode + 1]) : 1000;
        struct tally *t = calloc(1, sizeof *t);
        for (long i = 0; i < n; i++) pass(mode, t);
        report(mode, t);
        free(t);
    }
    return 0;
}
