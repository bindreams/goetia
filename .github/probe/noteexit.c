/* Probe: can a successful kqueue EV_ADD of EVFILT_PROC NOTE_EXIT miss the exit, for a pid that is not
 * the caller's child?  Modes:
 *   single  <passes> <D_us> <dmin_us> <dmax_us> <exitstatus 0|1>
 *   witness <passes> <D_us> <lead_us>
 *   zombie  <passes> <exitstatus 0|1>
 *   launchd <passes> <max_delay_us> <exitstatus 0|1>
 */
#define _DARWIN_C_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <libproc.h>
#include <mach/mach_time.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/mman.h>
#include <sys/proc_info.h>
#include <sys/resource.h>
#include <sys/sysctl.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef NOTE_EXITSTATUS
#define NOTE_EXITSTATUS 0x04000000
#endif
#define P_WEXIT_BIT 0x00002000

#define LABEL "goetia.probe.noteexit"
#define SYS "system/" LABEL
#define PLIST "/Library/LaunchDaemons/" LABEL ".plist"
#define LAUNCHCTL "/bin/launchctl"

/* ---- output ---- */

static void say(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
}

#define MAXKEY 240
static struct {
    char key[MAXKEY];
    long n;
} tal[4096];
static int ntal;

static void tally(const char *fmt, ...) {
    char k[MAXKEY];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(k, sizeof k, fmt, ap);
    va_end(ap);
    for (int i = 0; i < ntal; i++)
        if (!strcmp(tal[i].key, k)) {
            tal[i].n++;
            return;
        }
    if (ntal == 4096) return;
    strcpy(tal[ntal].key, k);
    tal[ntal++].n = 1;
}

static int cmp_tal(const void *a, const void *b) {
    return strcmp(((const typeof(tal[0]) *)a)->key, ((const typeof(tal[0]) *)b)->key);
}

static void dump(const char *title) {
    qsort(tal, ntal, sizeof tal[0], cmp_tal);
    say("===== %s =====", title);
    for (int i = 0; i < ntal; i++) say("%8ld  %s", tal[i].n, tal[i].key);
    ntal = 0;
}

static int detail_lines;
static void detail(const char *fmt, ...) {
    if (detail_lines++ >= 60) return;
    va_list ap;
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
}

/* ---- time ---- */

static mach_timebase_info_data_t tbi;
static int64_t ns_of(int64_t t) { return (int64_t)((__int128)t * tbi.numer / tbi.denom); }
static int64_t ticks_of(int64_t n) { return (int64_t)((__int128)n * tbi.denom / tbi.numer); }
static int64_t dns(uint64_t a, uint64_t b) { return ns_of((int64_t)a - (int64_t)b); }

static uint64_t rng_state = 0x9e3779b97f4a7c15ull;
static uint64_t rnd(void) {
    rng_state ^= rng_state >> 12;
    rng_state ^= rng_state << 25;
    rng_state ^= rng_state >> 27;
    return rng_state * 0x2545F4914F6CDD1Dull;
}

static void spin_until(int64_t aim) {
    while ((int64_t)mach_absolute_time() < aim) {}
}

/* ---- the kernel's view of a pid ---- */

struct kst {
    int kind; /* 0 readable, 1 gone, 2 error */
    int flag, stat;
};

static struct kst kinfo(pid_t p) {
    struct kst r = {2, 0, 0};
    int mib[4] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, (int)p};
    struct kinfo_proc kp;
    size_t sz;
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
    r.flag = kp.kp_proc.p_flag;
    r.stat = kp.kp_proc.p_stat;
    return r;
}

static int bsd_status(pid_t p) {
    struct proc_bsdinfo b;
    int n = proc_pidinfo(p, PROC_PIDTBSDINFO, 0, &b, sizeof b);
    return n == (int)sizeof b ? (int)b.pbi_status : -1;
}

static const char *state_label(pid_t p, char *buf, size_t n) {
    struct kst s = kinfo(p);
    int b = bsd_status(p);
    if (s.kind == 1)
        snprintf(buf, n, "gone(sysctl) pidinfo=%d", b);
    else if (s.kind == 2)
        snprintf(buf, n, "sysctl-error pidinfo=%d", b);
    else {
        const char *nm = s.stat == 1 ? "SIDL" : s.stat == 2 ? "SRUN" : s.stat == 3 ? "SSLEEP" : s.stat == 4 ? "SSTOP" : s.stat == 5 ? "SZOMB" : "S?";
        snprintf(buf, n, "%s%s pidinfo=%d", nm, (s.flag & P_WEXIT_BIT) ? "+WEXIT" : "", b);
    }
    return buf;
}

/* ---- kqueue ---- */

static int add_exit(int kq, pid_t pid, int es) {
    struct kevent ch;
    EV_SET(&ch, (uintptr_t)pid, EVFILT_PROC, EV_ADD | EV_CLEAR, NOTE_EXIT | (es ? NOTE_EXITSTATUS : 0), 0, NULL);
    for (;;) {
        int r = kevent(kq, &ch, 1, NULL, 0, NULL);
        if (r == 0) return 0;
        if (errno == EINTR) continue;
        return errno;
    }
}

/* One zero-timeout look at the queue: 1 if NOTE_EXIT for pid is in it, 0 if not, -1 on error. */
static int poll_exit(int kq, pid_t pid, int64_t *data) {
    struct kevent ev[8];
    struct timespec z = {0, 0};
    int n = kevent(kq, NULL, 0, ev, 8, &z);
    if (n < 0) return -1;
    int got = 0;
    for (int i = 0; i < n; i++)
        if (ev[i].filter == EVFILT_PROC && (pid_t)ev[i].ident == pid && (ev[i].fflags & NOTE_EXIT)) {
            got = 1;
            if (data) *data = (int64_t)ev[i].data;
        }
    return got;
}

/* ---- the helper: parent of the target ---- */

struct slot {
    uint64_t start, e;
};
static struct slot *slot;
static int cmdp[2], pidp[2], repp[2];
static int64_t D_ticks;

static void helper(int zombie) {
    close(cmdp[1]);
    close(pidp[0]);
    close(repp[0]);
    for (;;) {
        char c;
        ssize_t n = read(cmdp[0], &c, 1);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0 || c == 'q') _exit(0);
        pid_t t = fork();
        if (t < 0) _exit(3);
        if (t == 0) {
            uint64_t s = mach_absolute_time();
            __atomic_store_n(&slot->start, s, __ATOMIC_RELEASE);
            int64_t tg = (int64_t)s + D_ticks;
            spin_until(tg);
            __atomic_store_n(&slot->e, mach_absolute_time(), __ATOMIC_RELEASE);
            _exit(7);
        }
        write(pidp[1], &t, sizeof t);
        if (zombie) {
            do n = read(cmdp[0], &c, 1);
            while (n < 0 && errno == EINTR);
        }
        int st = 0;
        while (waitpid(t, &st, 0) < 0 && errno == EINTR) {}
        write(repp[1], &st, sizeof st);
    }
}

static void helper_start(int zombie) {
    slot = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANON, -1, 0);
    if (slot == MAP_FAILED) {
        perror("mmap");
        exit(2);
    }
    if (pipe(cmdp) || pipe(pidp) || pipe(repp)) {
        perror("pipe");
        exit(2);
    }
    pid_t h = fork();
    if (h < 0) {
        perror("fork");
        exit(2);
    }
    if (h == 0) helper(zombie);
    close(cmdp[0]);
    close(pidp[1]);
    close(repp[1]);
}

static pid_t begin_pass(uint64_t *start) {
    __atomic_store_n(&slot->start, 0, __ATOMIC_RELEASE);
    __atomic_store_n(&slot->e, 0, __ATOMIC_RELEASE);
    char c = 'g';
    if (write(cmdp[1], &c, 1) != 1) {
        perror("write cmd");
        exit(2);
    }
    pid_t pid;
    if (read(pidp[0], &pid, sizeof pid) != sizeof pid) {
        perror("read pid");
        exit(2);
    }
    uint64_t s;
    while ((s = __atomic_load_n(&slot->start, __ATOMIC_ACQUIRE)) == 0) {}
    *start = s;
    return pid;
}

static void wait_reaped(void) {
    int st;
    if (read(repp[0], &st, sizeof st) != sizeof st) {
        perror("read rep");
        exit(2);
    }
}

/* ---- classes and the offset histogram ---- */

enum { OK_DELIVERED, OK_MISSED, ESRCH_, OTHER, NCLASS };
static const char *clsname[NCLASS] = {"ok+delivered", "OK+NOT-DELIVERED", "ESRCH", "other-errno"};
static const int64_t edges_us[] = {-600, -300, -200, -100, -60, -40, -30, -20, -10, -5, 0, 5, 10, 20, 30, 40, 60, 100, 200, 300, 600};
#define NE ((int)(sizeof edges_us / sizeof edges_us[0]))
static long hist[NE + 1][NCLASS];

static int bucket_of(int64_t off_ns) {
    int b = 0;
    while (b < NE && off_ns >= edges_us[b] * 1000) b++;
    return b;
}

static void dump_hist(const char *what) {
    say("===== %s: outcome by (start of the EV_ADD call) - (the target's _exit call), in microseconds =====", what);
    say("%-22s %10s %10s %10s %10s", "offset bucket", clsname[0], clsname[1], clsname[2], clsname[3]);
    for (int b = 0; b <= NE; b++) {
        char lab[64];
        if (b == 0)
            snprintf(lab, sizeof lab, "< %lld", (long long)edges_us[0]);
        else if (b == NE)
            snprintf(lab, sizeof lab, ">= %lld", (long long)edges_us[NE - 1]);
        else
            snprintf(lab, sizeof lab, "[%lld, %lld)", (long long)edges_us[b - 1], (long long)edges_us[b]);
        say("%-22s %10ld %10ld %10ld %10ld", lab, hist[b][0], hist[b][1], hist[b][2], hist[b][3]);
    }
    memset(hist, 0, sizeof hist);
}

static int class_of(int err, int got) {
    if (err == 0) return got == 1 ? OK_DELIVERED : OK_MISSED;
    return err == ESRCH ? ESRCH_ : OTHER;
}

/* ---- mode single ---- */

static void single_pass(int i, int es, int64_t dmin_ns, int64_t dmax_ns) {
    uint64_t s;
    pid_t pid = begin_pass(&s);
    int64_t delta = dmin_ns + (int64_t)(rnd() % (uint64_t)(dmax_ns - dmin_ns + 1));
    int kq = kqueue();
    spin_until((int64_t)s + D_ticks + ticks_of(delta));
    uint64_t r0 = mach_absolute_time();
    int err = add_exit(kq, pid, es);
    uint64_t r1 = mach_absolute_time();
    char st[96];
    state_label(pid, st, sizeof st);
    wait_reaped();
    uint64_t e = __atomic_load_n(&slot->e, __ATOMIC_ACQUIRE);
    int64_t data = 0;
    int got = err ? 0 : poll_exit(kq, pid, &data);
    close(kq);
    int cls = class_of(err, got);
    int64_t off0 = dns(r0, e), off1 = dns(r1, e);
    hist[bucket_of(off0)][cls]++;
    if (err == 0 && got == 1 && es) tally("delivered status is 7<<8 (1792): %s", data == 1792 ? "yes" : "NO");
    tally("%s; state right after the call: %s", cls == OTHER ? "other errno" : clsname[cls], st);
    if (cls == OTHER) tally("other errno %d", err);
    if (cls == OK_MISSED) detail("MISSED pass %d: r0-e=%lld ns r1-e=%lld ns state=%s", i, (long long)off0, (long long)off1, st);
    if (r0 <= e && e <= r1) tally("the EV_ADD call straddles the target's _exit call: %s", clsname[cls]);
}

static void run_single(int passes, int64_t d_us, int64_t dmin_us, int64_t dmax_us, int es) {
    D_ticks = ticks_of(d_us * 1000);
    helper_start(0);
    for (int i = 0; i < passes; i++) {
        single_pass(i, es, dmin_us * 1000, dmax_us * 1000);
        if (i % 20000 == 19999) say("... %d passes", i + 1);
    }
    char title[160];
    snprintf(title, sizeof title, "single passes=%d D=%lldus offset in [%lld,%lld]us exitstatus=%d", passes, (long long)d_us, (long long)dmin_us,
             (long long)dmax_us, es);
    dump_hist(title);
    dump(title);
}

/* ---- mode witness ---- */

struct att {
    int kq, err, pre, post;
    char st[96];
};

static int peek_witness(int kqa, pid_t pid, int *seen) {
    if (!*seen) {
        int g = poll_exit(kqa, pid, NULL);
        if (g == 1) *seen = 1;
    }
    return *seen;
}

static void witness_pass(int es, int64_t lead_ns) {
    uint64_t s;
    pid_t pid = begin_pass(&s);
    int kqa = kqueue();
    int ea = add_exit(kqa, pid, 0);
    if (ea) {
        tally("witness could not register (errno %d): pass skipped", ea);
        wait_reaped();
        close(kqa);
        return;
    }
    int seen = 0;
    int cap = 256, n = 0;
    struct att *at = malloc(cap * sizeof *at);
    spin_until((int64_t)s + D_ticks - ticks_of(lead_ns));
    for (;;) {
        if (n == cap) {
            cap *= 2;
            at = realloc(at, cap * sizeof *at);
        }
        struct att *a = &at[n];
        a->pre = peek_witness(kqa, pid, &seen);
        a->kq = kqueue();
        a->err = add_exit(a->kq, pid, es);
        a->post = peek_witness(kqa, pid, &seen);
        state_label(pid, a->st, sizeof a->st);
        n++;
        if (a->err) break;
    }
    wait_reaped();
    int final_seen = seen;
    if (!seen) final_seen = poll_exit(kqa, pid, NULL) == 1;
    tally("witness had NOTE_EXIT queued once the target was reaped: %s", final_seen ? "yes" : "NO");
    tally("attempts per pass: %s", n < 5 ? "<5" : n < 20 ? "5-19" : n < 100 ? "20-99" : ">=100");
    for (int j = 0; j < n; j++) {
        struct att *a = &at[j];
        int64_t data;
        int got = a->err ? 0 : poll_exit(a->kq, pid, &data);
        if (a->err == 0 && got == 1 && es && data != 1792) tally("delivered status is NOT 7<<8");
        if (a->err == 0 && got == 1)
            tally("B ok, delivered;     witness readable before the call: %d, after: %d", a->pre, a->post);
        else if (a->err == 0)
            tally("B ok, NOT DELIVERED; witness readable before the call: %d, after: %d  <-- a missed exit", a->pre, a->post);
        else if (a->err == ESRCH)
            tally("B ESRCH;             witness readable before the call: %d, after: %d; state after: %s", a->pre, a->post, a->st);
        else
            tally("B errno %d;           witness readable before the call: %d, after: %d", a->err, a->pre, a->post);
        close(a->kq);
    }
    free(at);
    close(kqa);
}

static void run_witness(int passes, int64_t d_us, int64_t lead_us) {
    D_ticks = ticks_of(d_us * 1000);
    helper_start(0);
    for (int i = 0; i < passes; i++) {
        witness_pass(0, lead_us * 1000);
        if (i % 5000 == 4999) say("... %d passes", i + 1);
    }
    char title[160];
    snprintf(title, sizeof title, "witness passes=%d D=%lldus lead=%lldus", passes, (long long)d_us, (long long)lead_us);
    dump(title);
}

/* ---- mode zombie ---- */

static void run_zombie(int passes, int es) {
    D_ticks = 0;
    helper_start(1);
    for (int i = 0; i < passes; i++) {
        uint64_t s;
        pid_t pid = begin_pass(&s);
        struct kst k;
        do k = kinfo(pid);
        while (k.kind == 0 && k.stat != 5);
        char st0[96], st1[96];
        state_label(pid, st0, sizeof st0);
        int kq = kqueue();
        int err = add_exit(kq, pid, es);
        state_label(pid, st1, sizeof st1);
        int64_t data = 0;
        int got1 = err ? 0 : poll_exit(kq, pid, &data);
        char c = 'r';
        write(cmdp[1], &c, 1);
        wait_reaped();
        int got2 = (err || got1) ? 0 : poll_exit(kq, pid, &data);
        close(kq);
        tally("zombie target (parent has not reaped it): before the call %s; EV_ADD %s; NOTE_EXIT queued at once: %d; arriving by the time of the reap: %d",
              st0, err == 0 ? "ok" : (err == ESRCH ? "ESRCH" : "other"), got1, got2);
        if (err == 0 && got1 && es) tally("zombie: delivered status is 7<<8: %s", data == 1792 ? "yes" : "NO");
        if (err && err != ESRCH) tally("zombie: other errno %d", err);
        (void)st1;
    }
    char title[96];
    snprintf(title, sizeof title, "zombie passes=%d exitstatus=%d", passes, es);
    dump(title);
}

/* ---- mode launchd ---- */

struct cmd {
    int code;
    char out[4096], err[4096];
};

static void lc(struct cmd *c, ...) {
    char *argv[16];
    int n = 0;
    argv[n++] = LAUNCHCTL;
    va_list ap;
    va_start(ap, c);
    char *a;
    while ((a = va_arg(ap, char *)) != NULL && n < 15) argv[n++] = a;
    va_end(ap);
    argv[n] = NULL;
    int po[2], pe[2];
    if (pipe(po) || pipe(pe)) {
        perror("pipe");
        exit(2);
    }
    pid_t p = fork();
    if (p == 0) {
        dup2(po[1], 1);
        dup2(pe[1], 2);
        close(po[0]);
        close(po[1]);
        close(pe[0]);
        close(pe[1]);
        execv(LAUNCHCTL, argv);
        _exit(127);
    }
    close(po[1]);
    close(pe[1]);
    size_t len = 0;
    ssize_t r;
    while ((r = read(po[0], c->out + len, sizeof c->out - 1 - len)) > 0) len += (size_t)r;
    c->out[len] = 0;
    len = 0;
    while ((r = read(pe[0], c->err + len, sizeof c->err - 1 - len)) > 0) len += (size_t)r;
    c->err[len] = 0;
    close(po[0]);
    close(pe[0]);
    int st = 0;
    while (waitpid(p, &st, 0) < 0 && errno == EINTR) {}
    c->code = WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

static void write_plist(void) {
    FILE *f = fopen(PLIST, "w");
    if (!f) {
        perror("plist");
        exit(2);
    }
    fputs("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" "
          "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n"
          "<key>Label</key><string>" LABEL "</string>\n"
          "<key>ProgramArguments</key><array><string>/usr/bin/true</string></array>\n"
          "<key>UserName</key><string>root</string>\n"
          "<key>ThrottleInterval</key><integer>1</integer>\n"
          "</dict></plist>\n",
          f);
    fclose(f);
    chown(PLIST, 0, 0);
    chmod(PLIST, 0644);
}

static void bootstrap(void) {
    struct cmd c;
    long tries = 0;
    for (;; tries++) {
        lc(&c, "bootstrap", "system", PLIST, NULL);
        if (c.code == 0) break;
    }
    if (tries) tally("bootstrap needed a retry: %s", tries < 10 ? "<10 retries" : ">=10 retries");
}

static void uninstall(void) {
    struct cmd c;
    lc(&c, "bootout", SYS, NULL);
    unlink(PLIST);
}

static const int64_t ledges_us[] = {250, 500, 750, 1000, 1500, 2000, 3000, 4000, 6000, 8000, 12000};
#define NLE ((int)(sizeof ledges_us / sizeof ledges_us[0]))
static long lhist[NLE + 1][NCLASS];

static void run_launchd(int passes, int64_t max_us, int es) {
    struct cmd c;
    lc(&c, "bootout", SYS, NULL);
    write_plist();
    bootstrap();
    for (int i = 0; i < passes; i++) {
        lc(&c, "bootout", SYS, NULL);
        bootstrap();
        struct cmd k;
        lc(&k, "kickstart", "-p", SYS, NULL);
        pid_t pid = (pid_t)atol(k.out);
        if (k.code != 0 || pid <= 1) {
            tally("kickstart -p gave no pid: exit %d", k.code);
            continue;
        }
        int64_t delay = (int64_t)(rnd() % (uint64_t)(max_us + 1)) * 1000;
        int kq = kqueue();
        spin_until((int64_t)mach_absolute_time() + ticks_of(delay));
        int err = add_exit(kq, pid, es);
        char st[96];
        state_label(pid, st, sizeof st);
        int got = 0;
        int64_t data = 0;
        if (err == 0) {
            /* The target is certainly gone once kill(pid, 0) fails: launchd has reaped it. */
            while (kill(pid, 0) == 0) {}
            got = poll_exit(kq, pid, &data);
        }
        close(kq);
        int cls = class_of(err, got);
        int b = 0;
        while (b < NLE && delay >= ledges_us[b] * 1000) b++;
        lhist[b][cls]++;
        tally("%s; state right after the call: %s", cls == OTHER ? "other errno" : clsname[cls], st);
        if (err == 0 && got == 1 && es) tally("delivered status is 0: %s", data == 0 ? "yes" : "NO");
        if (cls == OTHER) tally("other errno %d", err);
        if (cls == OK_MISSED) detail("MISSED pass %d: delay %lld us state=%s", i, (long long)(delay / 1000), st);
        if (i % 500 == 499) say("... %d passes", i + 1);
    }
    char title[128];
    snprintf(title, sizeof title, "launchd passes=%d max_delay=%lldus exitstatus=%d", passes, (long long)max_us, es);
    say("===== %s: outcome by delay between kickstart -p's return and the EV_ADD, in microseconds =====", title);
    say("%-22s %10s %10s %10s %10s", "delay bucket", clsname[0], clsname[1], clsname[2], clsname[3]);
    for (int b = 0; b <= NLE; b++) {
        char lab[64];
        if (b == 0)
            snprintf(lab, sizeof lab, "< %lld", (long long)ledges_us[0]);
        else if (b == NLE)
            snprintf(lab, sizeof lab, ">= %lld", (long long)ledges_us[NLE - 1]);
        else
            snprintf(lab, sizeof lab, "[%lld, %lld)", (long long)ledges_us[b - 1], (long long)ledges_us[b]);
        say("%-22s %10ld %10ld %10ld %10ld", lab, lhist[b][0], lhist[b][1], lhist[b][2], lhist[b][3]);
    }
    dump(title);
    uninstall();
}

/* ---- main ---- */

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    mach_timebase_info(&tbi);
    rng_state ^= (uint64_t)time(NULL) * 0x100000001b3ull ^ (uint64_t)getpid();
    struct rlimit rl;
    if (getrlimit(RLIMIT_NOFILE, &rl) == 0) {
        rl.rlim_cur = rl.rlim_max < 10240 ? rl.rlim_max : 10240;
        setrlimit(RLIMIT_NOFILE, &rl);
    }
    if (argc < 2) {
        fprintf(stderr, "usage: see the top of the file\n");
        return 2;
    }
    say("uid %d, timebase %u/%u", (int)getuid(), tbi.numer, tbi.denom);
    if (!strcmp(argv[1], "single") && argc == 7)
        run_single(atoi(argv[2]), atoll(argv[3]), atoll(argv[4]), atoll(argv[5]), atoi(argv[6]));
    else if (!strcmp(argv[1], "witness") && argc == 5)
        run_witness(atoi(argv[2]), atoll(argv[3]), atoll(argv[4]));
    else if (!strcmp(argv[1], "zombie") && argc == 4)
        run_zombie(atoi(argv[2]), atoi(argv[3]));
    else if (!strcmp(argv[1], "launchd") && argc == 5)
        run_launchd(atoi(argv[2]), atoll(argv[3]), atoi(argv[4]));
    else {
        fprintf(stderr, "bad arguments\n");
        return 2;
    }
    return 0;
}
