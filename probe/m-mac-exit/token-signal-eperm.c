/* Claim: proc_signal_with_audittoken answers EPERM, once, to a child that is exiting and not yet
 *        reaped, between the last 0 (delivered) and the ESRCH that follows for good. kill(pid, 0)
 *        never answers EPERM to the same child. The token call also answers EINVAL to signal 0, so
 *        SIGCONT is the probe signal (harmless to a running child); modes 3-6 send SIGKILL.
 *        At that EPERM, kinfo_proc (sysctl KERN_PROC_PID) reads back readable with P_WEXIT set and
 *        p_stat SRUN: never SZOMB, never clear, never gone. It is read before and after every send.
 *
 * Verified: 2026-11-15 on a GitHub Actions macos-latest runner, macOS 26.7.2 (25G332), arm64,
 *           as root and as the runner user (uid 501).
 * Backs: [[macOS - proc_signal_with_audittoken answers EPERM to an exiting child before ESRCH]]
 *
 * Build: cc -O1 -Wall -o kill-eperm macos-token-signal-eperm-exiting-child.c
 * Run:   ./kill-eperm 3000 3000 1000 1000 30000 30000 5000
 *        (passes for mode 0: fork, sleep under 3 ms, _exit; mode 1: exec /usr/bin/true;
 *         mode 2: exec /bin/launchctl print system; mode 3: as mode 2, SIGKILL after a random
 *         0-60 ms delay; modes 4-6 model a group sweep: kill(pid, SIGKILL) first, then the token
 *         SIGKILL in a tight loop; mode 4: forked child in pause(); mode 5: exec /bin/sleep;
 *         mode 6: a child with 128 MB touched and 8 threads)
 * Expect: per mode, "a run of 1 EPERM answers, then another answer: N times" with N > 0 for modes 0-2
 *         (an earlier 2000/2000/500-pass run as root: 34, 76, 11; as uid 501: 31, 88, 20), about 1
 *         in 1000 or fewer for mode 3, no run longer than 1, and "passes with a kill() EPERM: 0".
 *         Over all modes and both users (2026-11-15: 544 EPERMs, 328 as uid 501, 216 as root; modes
 *         4-6 as uid 501: 81, 98, 56; as root: 60, 45, 78) the post-send read is "begun" at every
 *         EPERM: "of the post-send reads: P_WEXIT only: N" and "post-send p_stat=2 at EPERM: N"
 *         equal the EPERM total, and the classes "post-send read: CLEAR", "gone" and "sysctl error"
 *         print no line. The class "pre-send read: clear; post-send read: begun" may be non-zero
 *         (5 of 544): the flag was set between that read and the send.
 *         Mode 1 may also print GUARD lines: a child still running after 100000 sends, not a finding.
 * Expect-Match: a run of 1 EPERM answers, then another answer:
 */
#include <errno.h>
#include <fcntl.h>
#include <libproc.h>
#include <mach/message.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/proc_info.h>
#include <sys/sysctl.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define P_WEXIT_BIT 0x00002000

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

/* kinfo reads: 0 = readable, 1 = gone (size 0), 2 = sysctl error. */
struct kread {
    int kind;
    int p_flag;
    int p_stat;
};

static struct kread read_kinfo(pid_t p) {
    struct kread r = {2, 0, 0};
    int mib[4] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, (int)p};
    struct kinfo_proc kp;
    size_t sz = sizeof kp;
    memset(&kp, 0, sizeof kp);
    for (;;) {
        sz = sizeof kp;
        if (sysctl(mib, 4, &kp, &sz, NULL, 0) == 0) break;
        if (errno == EINTR) continue;
        return r;
    }
    if (sz == 0) {
        r.kind = 1;
        return r;
    }
    r.kind = 0;
    r.p_flag = kp.kp_proc.p_flag;
    r.p_stat = kp.kp_proc.p_stat;
    return r;
}

/* exit begun as cosca reads it: P_WEXIT set, or SZOMB (5). */
static int begun(struct kread r) { return r.kind == 0 && ((r.p_flag & P_WEXIT_BIT) || r.p_stat == 5); }

#define MAXERR 140
/* EPERM classes by (pre-send read, post-send read). */
enum { C_BOTH_BEGUN, C_PRE_CLEAR_POST_BEGUN, C_POST_CLEAR, C_POST_GONE, C_POST_ERR, C_NCLASS };
static const char *cname[C_NCLASS] = {
    "pre-send read: exit begun (P_WEXIT or SZOMB); post-send read: begun",
    "pre-send read: clear; post-send read: begun",
    "post-send read: CLEAR (P_WEXIT not set, not SZOMB)  <-- would break the premise",
    "post-send read: gone (sysctl size 0)",
    "post-send read: sysctl error",
};

struct tally {
    long passes;
    long token[MAXERR];
    long killz[MAXERR];
    long streak_hist[MAXERR];
    long gone_kill[MAXERR];
    long zombie_token[MAXERR], zombie_kill[MAXERR];
    long eperm_passes;
    long eperm_status[8];
    long ended_zombie, ended_gone, never_identified, kill_eperm_passes;
    int samples, guard_samples;
    long guard_hits, esrch_zombie;
    long eperm_total;
    long eperm_class[C_NCLASS];
    long eperm_wexit_and_szomb, eperm_wexit_only, eperm_szomb_only;
    long eperm_pstat[16];
    int violations;
};

static void bump(long *a, int e) { a[(e >= 0 && e < MAXERR) ? e : MAXERR - 1]++; }

static void *spin(void *arg) {
    (void)arg;
    for (;;) pause();
    return 0;
}

static void pass(int mode, struct tally *t) {
    pid_t p = fork();
    if (p == 0) {
        if (mode == 0) {
            usleep(rand() % 3000);
            _exit(0);
        }
        if (mode == 1) execl("/usr/bin/true", "true", (char *)0);
        if (mode == 2 || mode == 3) {
            int dn = open("/dev/null", O_WRONLY);
            dup2(dn, 1);
            dup2(dn, 2);
            execl("/bin/launchctl", "launchctl", "print", "system", (char *)0);
        }
        if (mode == 4) {
            for (;;) pause();
        }
        if (mode == 5) execl("/bin/sleep", "sleep", "1000", (char *)0);
        if (mode == 6) { /* a bigger teardown: 128 MB touched, 8 threads */
            size_t n = 128u << 20;
            char *m = malloc(n);
            if (m) memset(m, 1, n);
            for (int i = 0; i < 8; i++) {
                pthread_t th;
                pthread_create(&th, 0, spin, 0);
            }
            for (;;) pause();
        }
        _exit(127);
    }
    t->passes++;
    struct uniqid u;
    if (read_uniq(p, &u) != 0) {
        t->never_identified++;
        if (mode >= 4) kill(p, SIGKILL);
        waitpid(p, 0, 0);
        return;
    }
    uint64_t unique = u.p_uniqueid;
    uint32_t ver = (uint32_t)u.p_idversion;
    int sig = (mode == 3 || mode >= 4) ? SIGKILL : SIGCONT;
    if (mode == 3) usleep(rand() % 60000);
    if (mode >= 4) {
        if (mode == 6) usleep(20000); /* probe setup: let the child build its memory and threads */
        kill(p, SIGKILL); /* the group sweep's first send */
    }
    int saw_eperm = 0, saw_kill_eperm = 0;
    char run[256];
    size_t rl = 0;
    int last = -2;
    long iters = 0, streak = 0;
    run[0] = 0;
    for (;;) {
        audit_token_t tok = make_token(p, ver);
        struct kread pre = read_kinfo(p);
        int e = proc_signal_with_audittoken(&tok, sig);
        struct kread post = read_kinfo(p);
        int k = kill(p, 0) == 0 ? 0 : errno;
        int st0 = read_status(p);
        bump(t->token, e);
        if (e == EPERM) {
            t->eperm_total++;
            int c;
            if (post.kind == 1) c = C_POST_GONE;
            else if (post.kind == 2) c = C_POST_ERR;
            else if (!begun(post)) c = C_POST_CLEAR;
            else if (begun(pre)) c = C_BOTH_BEGUN;
            else c = C_PRE_CLEAR_POST_BEGUN;
            t->eperm_class[c]++;
            if (post.kind == 0) {
                int w = (post.p_flag & P_WEXIT_BIT) != 0, z = post.p_stat == 5;
                if (w && z) t->eperm_wexit_and_szomb++;
                else if (w) t->eperm_wexit_only++;
                else if (z) t->eperm_szomb_only++;
                t->eperm_pstat[(post.p_stat >= 0 && post.p_stat < 16) ? post.p_stat : 15]++;
            }
            if ((c == C_POST_CLEAR || c == C_POST_ERR || c == C_POST_GONE || c == C_PRE_CLEAR_POST_BEGUN) &&
                t->violations < 20) {
                t->violations++;
                printf("  EPERM class %d (mode %d): pre kind=%d flag=%#x stat=%d; post kind=%d flag=%#x stat=%d; "
                       "pbi_status=%d answers so far: %s\n",
                       c, mode, pre.kind, pre.p_flag, pre.p_stat, post.kind, post.p_flag, post.p_stat, st0, run);
            }
        }
        if (st0 == 5) {
            bump(t->zombie_token, e);
            bump(t->zombie_kill, k);
            t->ended_zombie++;
            if (e == EPERM) saw_eperm = 1;
            break;
        }
        if (st0 < 0 && e != 0 && e != EPERM && e != ESRCH) break;
        if (++iters > 100000) {
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
                printf("  EPERM sample (mode %d): pbi_status=%d pre(kind=%d flag=%#x stat=%d) post(kind=%d flag=%#x "
                       "stat=%d) answers so far: %s\n",
                       mode, st, pre.kind, pre.p_flag, pre.p_stat, post.kind, post.p_flag, post.p_stat, run);
            }
        }
        if (e == EPERM) streak++;
        else if (streak) {
            bump(t->streak_hist, (int)streak);
            streak = 0;
        }
        if (e == 0) continue;
        if (e == ESRCH || e == EPERM) {
            struct uniqid now;
            int r = read_uniq(p, &now);
            if (r == 0 && now.p_uniqueid == unique) {
                if ((uint32_t)now.p_idversion != ver) { ver = (uint32_t)now.p_idversion; continue; }
                if (e == ESRCH) { t->esrch_zombie++; break; }
                continue;
            }
            if (e == EPERM) continue;
            t->ended_gone++;
            bump(t->gone_kill, k);
            break;
        }
        break;
    }
    if (saw_eperm) t->eperm_passes++;
    if (saw_kill_eperm) t->kill_eperm_passes++;
    if (mode >= 4) kill(p, SIGKILL);
    waitpid(p, 0, 0);
}

static void report(int mode, struct tally *t) {
    printf("mode %d: passes=%ld never_identified=%ld ended_zombie=%ld esrch_zombie=%ld ended_gone=%ld guard_hits=%ld\n",
           mode, t->passes, t->never_identified, t->ended_zombie, t->esrch_zombie, t->ended_gone, t->guard_hits);
    for (int e = 0; e < MAXERR; e++)
        if (t->token[e]) printf("  token-signal answer %3d (%s): %ld\n", e, e ? strerror(e) : "ok", t->token[e]);
    for (int e = 0; e < MAXERR; e++)
        if (t->killz[e]) printf("  kill(pid,0) answer  %3d (%s): %ld\n", e, e ? strerror(e) : "ok", t->killz[e]);
    for (int e = 0; e < MAXERR; e++) {
        if (t->zombie_token[e]) printf("  to a zombie: token-signal answer %3d (%s): %ld
", e, e ? strerror(e) : "ok", t->zombie_token[e]);
        if (t->zombie_kill[e]) printf("  to a zombie: kill(pid,0) answer  %3d (%s): %ld
", e, e ? strerror(e) : "ok", t->zombie_kill[e]);
    }
    for (int n = 1; n < MAXERR; n++)
        if (t->streak_hist[n]) printf("  a run of %d EPERM answers, then another answer: %ld times\n", n, t->streak_hist[n]);
    for (int e = 0; e < MAXERR; e++) {
        if (t->gone_kill[e]) printf("  at the ending ESRCH, kill(pid,0) answered %d: %ld times
", e, t->gone_kill[e]);
    }
    printf("  passes with a token-signal EPERM: %ld; passes with a kill() EPERM: %ld\n", t->eperm_passes,
           t->kill_eperm_passes);
    printf("  EPERM answers in total: %ld\n", t->eperm_total);
    for (int c = 0; c < C_NCLASS; c++)
        if (t->eperm_class[c]) printf("    %s: %ld\n", cname[c], t->eperm_class[c]);
    printf("    of the post-send reads: P_WEXIT and SZOMB: %ld; P_WEXIT only: %ld; SZOMB only: %ld\n",
           t->eperm_wexit_and_szomb, t->eperm_wexit_only, t->eperm_szomb_only);
    for (int s = 0; s < 16; s++)
        if (t->eperm_pstat[s]) printf("    post-send p_stat=%d at EPERM: %ld\n", s, t->eperm_pstat[s]);
    fflush(stdout);
}

int main(int argc, char **argv) {
    printf("uid=%d\n", (int)getuid());
    srand((unsigned)time(0));
    for (int mode = 0; mode < 7; mode++) {
        long n = argc > mode + 1 ? atol(argv[mode + 1]) : 1000;
        struct tally *t = calloc(1, sizeof *t);
        for (long i = 0; i < n; i++) pass(mode, t);
        report(mode, t);
        free(t);
    }
    return 0;
}
