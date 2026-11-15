/* Probe (temporary, attended): how a held launchd launch is told apart from an unheld one in the kernel.
 *
 * Run as root on a throwaway macOS machine. It installs one job, `goetia.probe.runhold`, and removes it.
 *
 *   run-hold held   <plain|wrap> <early|late> <b2b|spaced> <passes> [throttle]
 *   run-hold unheld <plain|wrap> <early|late> <b2b|spaced> <passes> [throttle]
 *   run-hold reply  <passes>
 *   run-hold race   <passes> <max-delay-us> <withprint 0|1>
 *
 * held/unheld (M1-M3): per pass, `launchctl debug system/<l> --start-suspended` (held only), then
 * `launchctl kickstart -p`; the pid is registered for NOTE_EXEC|NOTE_EXIT (early: while kickstart still runs;
 * late: after it exited, as goetia does), and its p_stat is read by sysctl(KERN_PROC_PID) and by
 * proc_pidinfo(PROC_PIDTBSDINFO) at the event and repeatedly after it. wrap = the /bin/sh wrapper goetia
 * installs for a task with logs; plain = the program itself. b2b = the next pass starts at once after
 * NOTE_EXIT, spaced = after `print` has stopped showing the pid.
 * reply/race (M4): what `kickstart -p` prints over a running job, and whether a process start time orders a
 * held launch against a `debug`.
 */
#define _DARWIN_C_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <libproc.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/proc_info.h>
#include <sys/resource.h>
#include <sys/sysctl.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

#define LABEL "goetia.probe.runhold"
#define SYS "system/" LABEL
#define PLIST "/Library/LaunchDaemons/" LABEL ".plist"
#define LOGFILE "/tmp/goetia-probe-runhold.log"
#define LAUNCHCTL "/bin/launchctl"

/* Process states, as in <sys/proc.h>. */
#define ST_RUN 2
#define ST_STOP 4

/* ---- output ---- */

static void say(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
}

static int anomalies;
static void anom(const char *fmt, ...) {
    if (anomalies++ >= 60) return;
    va_list ap;
    va_start(ap, fmt);
    printf("ANOMALY ");
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
}

#define MAXKEY 200
static struct {
    char key[MAXKEY];
    long n;
} tal[2048];
static int ntal;

/* Counts one occurrence of the formatted key; returns 1 the first time a key is seen. */
static int tally(const char *fmt, ...) {
    char k[MAXKEY];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(k, sizeof k, fmt, ap);
    va_end(ap);
    for (int i = 0; i < ntal; i++)
        if (!strcmp(tal[i].key, k)) {
            tal[i].n++;
            return 0;
        }
    if (ntal == 2048) return 0;
    strcpy(tal[ntal].key, k);
    tal[ntal++].n = 1;
    return 1;
}

static void dump(const char *title) {
    say("===== %s =====", title);
    for (int i = 0; i < ntal; i++) say("%7ld  %s", tal[i].n, tal[i].key);
    ntal = 0;
}

/* ---- time ---- */

static mach_timebase_info_data_t tbi;
static int64_t ns_of(uint64_t t) { return (int64_t)((__uint128_t)t * tbi.numer / tbi.denom); }
static int64_t diff_ns(uint64_t a, uint64_t b) { return ns_of(a > b ? a - b : b - a) * (a > b ? 1 : -1); }

static int64_t start_abstime(pid_t pid, uint64_t *out) {
    struct rusage_info_v0 ri;
    memset(&ri, 0, sizeof ri);
    if (proc_pid_rusage(pid, RUSAGE_INFO_V0, (rusage_info_t *)&ri) != 0) return errno ? errno : -1;
    *out = ri.ri_proc_start_abstime;
    return 0;
}

/* ---- running launchctl ---- */

struct cmd {
    int code;
    char out[16384];
    char err[4096];
};

static int spawn_pipes(char *const argv[], pid_t *pid, int *outfd, int *errfd) {
    int o[2], e[2];
    if (pipe(o) || pipe(e)) return -1;
    pid_t p = fork();
    if (p < 0) return -1;
    if (p == 0) {
        dup2(o[1], 1);
        dup2(e[1], 2);
        close(o[0]);
        close(o[1]);
        close(e[0]);
        close(e[1]);
        execv(argv[0], argv);
        _exit(127);
    }
    close(o[1]);
    close(e[1]);
    *pid = p;
    *outfd = o[0];
    *errfd = e[0];
    return 0;
}

static void slurp(int fd, char *buf, size_t cap, size_t have) {
    char scratch[512];
    ssize_t r;
    for (;;) {
        if (have + 1 < cap)
            r = read(fd, buf + have, cap - 1 - have);
        else
            r = read(fd, scratch, sizeof scratch);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) break;
        if (have + 1 < cap) have += (size_t)r;
    }
    buf[have] = 0;
    close(fd);
}

static int reap(pid_t p) {
    int st;
    while (waitpid(p, &st, 0) < 0 && errno == EINTR) {}
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

/* lc(&c, "print", SYS, NULL): runs launchctl with the arguments to the NULL. */
static int lc(struct cmd *c, ...) {
    char *argv[16];
    int n = 0;
    argv[n++] = LAUNCHCTL;
    va_list ap;
    va_start(ap, c);
    char *a;
    while ((a = va_arg(ap, char *))) argv[n++] = a;
    va_end(ap);
    argv[n] = NULL;
    pid_t p;
    int o, e;
    if (spawn_pipes(argv, &p, &o, &e)) {
        c->code = -1;
        c->out[0] = c->err[0] = 0;
        return -1;
    }
    slurp(o, c->out, sizeof c->out, 0);
    slurp(e, c->err, sizeof c->err, 0);
    c->code = reap(p);
    return c->code;
}

/* A `launchctl kickstart -p` whose pid is read the moment it is printed, before launchctl has exited. */
struct kp {
    pid_t child;
    int outfd, errfd;
    pid_t pid;
    char first[64];
};

static int kick_spawn(struct kp *k, int dash_k) {
    char *argv_p[] = {LAUNCHCTL, "kickstart", "-p", SYS, NULL};
    char *argv_kp[] = {LAUNCHCTL, "kickstart", "-k", "-p", SYS, NULL};
    return spawn_pipes(dash_k ? argv_kp : argv_p, &k->child, &k->outfd, &k->errfd);
}

static void kick_read_pid(struct kp *k) {
    size_t n = 0;
    char ch;
    for (;;) {
        ssize_t r = read(k->outfd, &ch, 1);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0 || ch == '\n') break;
        if (n + 1 < sizeof k->first) k->first[n++] = ch;
    }
    k->first[n] = 0;
    char *end;
    long v = strtol(k->first, &end, 10);
    k->pid = (end != k->first && v > 0) ? (pid_t)v : -1;
}

static void kick_finish(struct kp *k, struct cmd *c) {
    slurp(k->outfd, c->out, sizeof c->out, 0);
    slurp(k->errfd, c->err, sizeof c->err, 0);
    c->code = reap(k->child);
}

/* The value of the line `name`'s text in `out` (whitespace-trimmed start), or -1. */
static long field(const char *out, const char *name) {
    size_t len = strlen(name);
    for (const char *p = out; p && *p;) {
        const char *q = p;
        while (*q == ' ' || *q == '\t') q++;
        if (!strncmp(q, name, len)) return strtol(q + len, NULL, 10);
        p = strchr(p, '\n');
        if (p) p++;
    }
    return -1;
}

/* out with digit runs as N and newlines as |, for comparing replies across passes. */
static const char *shape(const char *out, char *buf, size_t cap) {
    size_t n = 0;
    for (const char *p = out; *p && n + 2 < cap; p++) {
        if (*p >= '0' && *p <= '9') {
            if (n == 0 || buf[n - 1] != 'N') buf[n++] = 'N';
        } else if (*p == '\n')
            buf[n++] = '|';
        else
            buf[n++] = *p;
    }
    buf[n] = 0;
    return buf;
}

/* ---- the job ---- */

static void xml_text(FILE *f, const char *s) {
    for (; *s; s++) {
        if (*s == '&')
            fputs("&amp;", f);
        else if (*s == '<')
            fputs("&lt;", f);
        else if (*s == '>')
            fputs("&gt;", f);
        else
            fputc(*s, f);
    }
}

static void arg_xml(FILE *f, const char *s) {
    fputs("<string>", f);
    xml_text(f, s);
    fputs("</string>", f);
}

static void write_plist_for(const char *label, const char *path, int wrap, const char *prog, int throttle) {
    FILE *f = fopen(path, "w");
    if (!f) {
        perror("plist");
        exit(2);
    }
    fputs("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" "
          "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n",
          f);
    fputs("<key>Label</key>", f);
    arg_xml(f, label);
    fputs("\n<key>ProgramArguments</key><array>", f);
    if (wrap) {
        arg_xml(f, "/bin/sh");
        arg_xml(f, "-c");
        arg_xml(f, "printf '%s %08x\\n' goetia-run $$; exec \"$0\" \"$@\"");
    }
    if (!strcmp(prog, "true")) {
        arg_xml(f, "/usr/bin/true");
    } else {
        arg_xml(f, "/bin/sleep");
        arg_xml(f, "600");
    }
    fputs("</array>\n<key>UserName</key><string>root</string>\n", f);
    fprintf(f, "<key>ThrottleInterval</key><integer>%d</integer>\n", throttle);
    if (wrap) fputs("<key>StandardOutPath</key><string>" LOGFILE "</string><key>StandardErrorPath</key><string>" LOGFILE "</string>\n", f);
    fputs("</dict></plist>\n", f);
    fclose(f);
    chown(path, 0, 0);
    chmod(path, 0644);
}

static void write_plist(int wrap, const char *prog, int throttle) { write_plist_for(LABEL, PLIST, wrap, prog, throttle); }

static void bootstrap_plist(const char *plist) {
    struct cmd c;
    for (long tries = 0;; tries++) {
        lc(&c, "bootstrap", "system", plist, NULL);
        if (c.code == 0) {
            if (tries) tally("bootstrap needed %s retries", tries < 10 ? "<10" : ">=10");
            return;
        }
        if (tries == 0) tally("bootstrap first try exit %d", c.code);
    }
}

static void bootstrap(void) { bootstrap_plist(PLIST); }

static void install(int wrap, const char *prog, int throttle) {
    struct cmd c;
    lc(&c, "bootout", SYS, NULL);
    write_plist(wrap, prog, throttle);
    bootstrap();
}

/* Ends any process, boots the job out and back in: clears a pending hold (measured elsewhere). */
static void reset(void) {
    struct cmd c;
    lc(&c, "kill", "SIGKILL", SYS, NULL);
    lc(&c, "bootout", SYS, NULL);
    bootstrap();
}

static void uninstall(void) {
    struct cmd c;
    lc(&c, "kill", "SIGKILL", SYS, NULL);
    lc(&c, "bootout", SYS, NULL);
    unlink(PLIST);
    unlink(LOGFILE);
}

/* ---- what the kernel says of a pid ---- */

struct snap {
    int sys_ok, sys_stat, sys_err;
    int bsd_ok, bsd_stat, bsd_err;
    int path_ok;
    char base[32];
};

static int sys_stat_of(pid_t pid, int *err) {
    int mib[4] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, pid};
    struct kinfo_proc kp;
    size_t len = sizeof kp;
    memset(&kp, 0, sizeof kp);
    if (sysctl(mib, 4, &kp, &len, NULL, 0) != 0) {
        *err = errno;
        return -1;
    }
    if (len == 0) {
        *err = ESRCH;
        return -1;
    }
    return kp.kp_proc.p_stat;
}

static int bsd_stat_of(pid_t pid, int *err) {
    struct proc_bsdinfo b;
    int n = proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &b, sizeof b);
    if (n != (int)sizeof b) {
        *err = errno ? errno : -1;
        return -1;
    }
    return (int)b.pbi_status;
}

static void take(pid_t pid, struct snap *s) {
    memset(s, 0, sizeof *s);
    s->sys_stat = sys_stat_of(pid, &s->sys_err);
    s->sys_ok = s->sys_stat >= 0;
    s->bsd_stat = bsd_stat_of(pid, &s->bsd_err);
    s->bsd_ok = s->bsd_stat >= 0;
    char path[PROC_PIDPATHINFO_MAXSIZE];
    s->path_ok = proc_pidpath(pid, path, sizeof path) > 0;
    if (s->path_ok) {
        const char *b = strrchr(path, '/');
        snprintf(s->base, sizeof s->base, "%s", b ? b + 1 : path);
    } else {
        snprintf(s->base, sizeof s->base, "(no path, errno %d)", errno);
    }
}

static const char *sname(int st) {
    static char buf[4][24];
    static int i;
    char *b = buf[i++ & 3];
    switch (st) {
    case 1: return "IDL";
    case 2: return "RUN";
    case 3: return "SLEEP";
    case 4: return "STOP";
    case 5: return "ZOMB";
    case -1: return "(read failed)";
    default: snprintf(b, 24, "?%d", st); return b;
    }
}

/* ---- kqueue ---- */

static int kq = -1;

static int reg(pid_t pid) {
    struct kevent ch, rc;
    EV_SET(&ch, pid, EVFILT_PROC, EV_ADD | EV_ENABLE | EV_CLEAR | EV_RECEIPT, NOTE_EXEC | NOTE_EXIT | NOTE_EXITSTATUS, 0, 0);
    struct timespec z = {0, 0};
    int n;
    do n = kevent(kq, &ch, 1, &rc, 1, &z);
    while (n < 0 && errno == EINTR);
    if (n < 0) return errno;
    if (n == 0) return -1;
    if (rc.flags & EV_ERROR) return (int)rc.data;
    return 0;
}

static void unreg(pid_t pid) {
    struct kevent ch;
    EV_SET(&ch, pid, EVFILT_PROC, EV_DELETE, 0, 0, 0);
    kevent(kq, &ch, 1, NULL, 0, NULL);
}

struct ev {
    uint32_t fflags;
    intptr_t data;
    pid_t pid;
};

/* One event, or 0 if none arrived in `ms`. The bound is this probe's failure bound, tallied as data. */
static int next_ev(struct ev *e, long ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    struct kevent k;
    int n;
    do n = kevent(kq, NULL, 0, &k, 1, &t);
    while (n < 0 && errno == EINTR);
    if (n <= 0) return 0;
    e->fflags = k.fflags;
    e->data = k.data;
    e->pid = (pid_t)k.ident;
    return 1;
}

static const char *evname(uint32_t f) {
    static char buf[4][48];
    static int i;
    char *b = buf[i++ & 3];
    snprintf(b, 48, "%s%s%s", (f & NOTE_EXEC) ? "EXEC" : "", ((f & NOTE_EXEC) && (f & NOTE_EXIT)) ? "|" : "",
             (f & NOTE_EXIT) ? "EXIT" : "");
    return b;
}

/* ---- M1-M3: held and unheld launches ---- */

#define LOOP_READS 2000

static int O_held, O_late, O_settle;
static pid_t prev_pid = -1;
static int64_t min_after_ta = INT64_MAX, max_before_tb = INT64_MIN;
static int taken;

static void settle(pid_t pid) {
    struct cmd c;
    for (;;) {
        lc(&c, "print", SYS, NULL);
        if (field(c.out, "pid = ") != pid) return;
    }
}

static void pass(int i) {
    struct cmd c;
    uint64_t tb = 0, ta = 0;
    if (O_held) {
        tb = mach_absolute_time();
        lc(&c, "debug", SYS, "--start-suspended", NULL);
        ta = mach_absolute_time();
        tally("debug exit %d", c.code);
        if (c.code) anom("pass %d: debug exit %d: %s", i, c.code, c.err);
    }
    struct kp k;
    if (kick_spawn(&k, 0)) {
        anom("pass %d: spawn kickstart failed", i);
        return;
    }
    kick_read_pid(&k);
    pid_t pid = k.pid;
    if (pid < 0) {
        kick_finish(&k, &c);
        tally("kickstart -p printed no pid (exit %d)", c.code);
        anom("pass %d: no pid: out [%s] err [%s]", i, c.out, c.err);
        return;
    }
    int rr;
    struct snap post;
    if (O_late) {
        kick_finish(&k, &c);
        rr = reg(pid);
        take(pid, &post);
    } else {
        rr = reg(pid);
        take(pid, &post);
        kick_finish(&k, &c);
    }
    tally("kickstart exit %d, stderr %s", c.code, c.err[0] ? "non-empty" : "empty");
    tally("pid equals the previous pass's: %s", pid == prev_pid ? "yes" : "no");
    prev_pid = pid;
    if (rr) {
        tally("register errno %d", rr);
        anom("pass %d: register pid %d: errno %d; snapshot path %s sys %s", i, pid, rr, post.base, sname(post.sys_stat));
        if (O_settle) settle(pid);
        return;
    }

    /* M4, ours: the start time of a launch this pass made, against the bracket of its debug. */
    if (O_held) {
        uint64_t st;
        if (start_abstime(pid, &st) == 0) {
            int64_t after = diff_ns(st, ta), before = diff_ns(tb, st);
            tally("start time vs debug bracket: %s", st > ta ? "after ta" : (st >= tb ? "inside" : "before tb"));
            if (after < min_after_ta) min_after_ta = after;
            if (before > max_before_tb) max_before_tb = before;
        } else
            tally("proc_pid_rusage failed");
    }

    int at_xpcproxy = post.path_ok && !strcmp(post.base, "xpcproxy");
    struct ev e;
    int got = next_ev(&e, at_xpcproxy ? 5000 : (O_held ? 250 : 150));
    int exec = got && (e.fflags & NOTE_EXEC);
    tally("registered; right after: path %s, sys %s, bsd %s; event %s", post.base, sname(post.sys_stat), sname(post.bsd_stat),
          got ? evname(e.fflags) : "none");
    if (!post.sys_ok || !post.bsd_ok) tally("read failed: sysctl errno %d, proc_pidinfo errno %d", post.sys_err, post.bsd_err);
    if (at_xpcproxy && !exec) anom("pass %d: at xpcproxy after registering, but no NOTE_EXEC (event %s)", i, got ? evname(e.fflags) : "none");

    int e_err = 0, b_err = 0;
    int want_stop = O_held;
    int s0 = sys_stat_of(pid, &e_err), b0 = bsd_stat_of(pid, &b_err);
    tally("%s, first read: sys %s, bsd %s", exec ? "after NOTE_EXEC" : "no NOTE_EXEC event", sname(s0), sname(b0));
    long wrong_s = 0, wrong_b = 0, fail = 0;
    int hist[8] = {0};
    for (int j = 0; j < LOOP_READS; j++) {
        int a = sys_stat_of(pid, &e_err), b = bsd_stat_of(pid, &b_err);
        if (a < 0 || b < 0) fail++;
        if (a >= 0 && a < 8) hist[a]++;
        if (want_stop ? (a != ST_STOP) : (a == ST_STOP)) wrong_s++;
        if (want_stop ? (b != ST_STOP) : (b == ST_STOP)) wrong_b++;
    }
    tally("%d repeated reads (%s): reads that were %s: sysctl %s, proc_pidinfo %s; failed reads %s", LOOP_READS,
          exec ? "after NOTE_EXEC" : "no NOTE_EXEC", want_stop ? "not stopped" : "stopped", wrong_s ? ">0" : "0",
          wrong_b ? ">0" : "0", fail ? ">0" : "0");
    if (wrong_s || wrong_b)
        anom("pass %d: %s reads of a %s run: sys wrong %ld, bsd wrong %ld, failed %ld (RUN %d STOP %d)", i, exec ? "post-exec" : "no-event",
             want_stop ? "held" : "unheld", wrong_s, wrong_b, fail, hist[2], hist[4]);
    if (!O_held) tally("unheld states over the repeated reads: RUN %s, SLEEP %s, STOP %s", hist[2] ? "y" : "n", hist[3] ? "y" : "n", hist[4] ? "y" : "n");

    /* M3: the task-port route, for comparison. */
    if (taken < 60) {
        taken++;
        mach_port_t t;
        kern_return_t kr = task_for_pid(mach_task_self(), pid, &t);
        tally("task_for_pid on %s: kr %d", post.base, kr);
        if (kr == KERN_SUCCESS) {
            struct task_basic_info ti;
            mach_msg_type_number_t cnt = TASK_BASIC_INFO_COUNT;
            if (task_info(t, TASK_BASIC_INFO, (task_info_t)&ti, &cnt) == KERN_SUCCESS)
                tally("task_info suspend_count %d (%s)", (int)ti.suspend_count, want_stop ? "held" : "unheld");
            mach_port_deallocate(mach_task_self(), t);
        }
        /* The same reads from an unprivileged account (uid 501): does the read need root? */
        pid_t u = fork();
        if (u == 0) {
            if (setgid(20) != 0 || setuid(501) != 0) _exit(255);
            int ee = 0;
            int a = sys_stat_of(pid, &ee), b = bsd_stat_of(pid, &ee);
            mach_port_t t2;
            int kr2 = task_for_pid(mach_task_self(), pid, &t2) == KERN_SUCCESS;
            _exit(((a & 7) << 5) | ((b & 7) << 2) | kr2); /* 7 = read failed (-1) */
        }
        int ucode = reap(u);
        if (ucode == 255) {
            tally("unprivileged child: could not drop to uid 501");
        } else {
            int ua = (ucode >> 5) & 7, ub = (ucode >> 2) & 7;
            tally("unprivileged (uid 501) reads: sysctl %s, proc_pidinfo %s, task_for_pid %s (%s)", sname(ua == 7 ? -1 : ua), sname(ub == 7 ? -1 : ub),
                  (ucode & 1) ? "ok" : "denied", want_stop ? "held" : "unheld");
        }
    }

    int exited = 0;
    if (O_held) {
        lc(&c, "kill", "SIGCONT", SYS, NULL);
        tally("kill SIGCONT exit %d", c.code);
    } else {
        lc(&c, "kill", "SIGKILL", SYS, NULL);
        tally("kill SIGKILL exit %d", c.code);
    }
    if (got && (e.fflags & NOTE_EXIT)) exited = 1;
    char seq[160] = "";
    for (int n = 0; !exited && n < 8; n++) {
        struct ev f;
        if (!next_ev(&f, 5000)) {
            strcat(seq, "(none in 5s)");
            break;
        }
        char one[48];
        snprintf(one, sizeof one, "%s%s", n ? "," : "", evname(f.fflags));
        strcat(seq, one);
        if (f.fflags & NOTE_EXIT) {
            exited = 1;
            int st = (int)f.data;
            snprintf(one, sizeof one, "(status %d)", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
            strcat(seq, one);
        }
    }
    tally("events after %s: %s", O_held ? "SIGCONT" : "SIGKILL", seq);
    if (!exited) {
        unreg(pid);
        lc(&c, "kill", "SIGKILL", SYS, NULL);
        anom("pass %d: no exit seen; killed", i);
    }
    struct ev s;
    while (next_ev(&s, 0)) tally("stray event after the pass: %s pid %d", evname(s.fflags), s.pid);
    if (O_settle) settle(pid);
}

static void sanity(void) {
    pid_t c = fork();
    if (c == 0) {
        execl("/bin/sleep", "sleep", "5", (char *)NULL);
        _exit(127);
    }
    uint64_t t1 = mach_absolute_time(), st = 0;
    int r = start_abstime(c, &st);
    uint64_t t2 = mach_absolute_time();
    uint64_t t0 = t1;
    say("sanity: child start time read %d; before-fork-ish %s", r, r ? "" : (st <= t2 ? "start <= now: units agree" : "START IN THE FUTURE"));
    (void)t0;
    kill(c, SIGKILL);
    reap(c);
}

static void run_passes(int held, int wrap, int late, int settle_flag, int passes, int throttle) {
    O_held = held;
    O_late = late;
    O_settle = settle_flag;
    prev_pid = -1;
    min_after_ta = INT64_MAX;
    max_before_tb = INT64_MIN;
    taken = 0;
    anomalies = 0;
    install(wrap, held ? "true" : "sleep", throttle);
    for (int i = 0; i < passes; i++) {
        pass(i);
        if (i % 100 == 99) say("... %d passes", i + 1);
    }
    char title[160];
    snprintf(title, sizeof title, "%s %s %s %s passes=%d throttle=%d", held ? "HELD" : "UNHELD", wrap ? "wrap(/bin/sh)" : "plain",
             late ? "late-register" : "early-register", settle_flag ? "spaced" : "back-to-back", passes, throttle);
    if (held && min_after_ta != INT64_MAX)
        say("M4 ours: smallest (start - ta) %lld us; largest (tb - start) over passes with start before tb %lld us", (long long)(min_after_ta / 1000),
            (long long)(max_before_tb / 1000));
    dump(title);
    uninstall();
}

/* ---- M4: replies and the start time ---- */

static void dump_state(const char *what) {
    struct cmd c;
    lc(&c, "print", SYS, NULL);
    say("----- print, %s -----\n%s", what, c.out);
    lc(&c, "blame", SYS, NULL);
    say("----- blame, %s (exit %d) -----\n%s%s", what, c.code, c.out, c.err);
}

static void reply_shape(const char *what, struct cmd *c) {
    char s[256], t[256];
    tally("%s: out [%s] err [%s] exit %d", what, shape(c->out, s, sizeof s), shape(c->err, t, sizeof t), c->code);
}

static void gone(pid_t p) {
    while (kill(p, 0) == 0) {}
}

static void reply(int i) {
    struct cmd c, x, y, z;
    reset();
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    tally("H0: debug exit %d", c.code);
    lc(&x, "kickstart", "-p", SYS, NULL);
    pid_t p = (pid_t)atol(x.out);
    reply_shape("start over a held pending hold", &x);
    if (i == 0) dump_state("after the held start (hold consumed)");
    uint64_t tb = mach_absolute_time();
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    uint64_t ta = mach_absolute_time();
    tally("our debug after a launch took the hold: exit %d", c.code);
    if (i == 0) dump_state("after our debug (a fresh hold pending, the older launch alive)");
    lc(&y, "kickstart", "-p", SYS, NULL);
    pid_t q = (pid_t)atol(y.out);
    reply_shape("kickstart -p over a running held launch", &y);
    tally("that reply names the older launch's pid: %s", q == p ? "yes" : "no");
    if (i == 0) dump_state("after kickstart -p over the running launch");
    uint64_t st;
    if (start_abstime(p, &st) == 0)
        tally("older held launch's start time vs our debug bracket: %s", st > ta ? "AFTER ta" : (st >= tb ? "INSIDE" : "before tb"));
    else
        tally("older launch: proc_pid_rusage failed");
    int e1 = 0;
    tally("older launch reads: sysctl %s", sname(sys_stat_of(p, &e1)));
    /* Does the fresh hold survive the older launch? D2 after it ends: 37 = still pending. */
    lc(&c, "kill", "SIGKILL", SYS, NULL);
    gone(p);
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    tally("debug after the older launch ended: exit %d (37 = our hold still pending)", c.code);
    /* -k -p over a running launch. */
    reset();
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    lc(&x, "kickstart", "-p", SYS, NULL);
    p = (pid_t)atol(x.out);
    lc(&z, "kickstart", "-k", "-p", SYS, NULL);
    pid_t r = (pid_t)atol(z.out);
    reply_shape("kickstart -k -p over a running held launch", &z);
    tally("-k -p names a new pid: %s", (r != p && r > 0) ? "yes" : "no");
    int e2 = 0;
    tally("the -k -p launch reads: sysctl %s", sname(sys_stat_of(r, &e2)));
    /* Unheld shapes. */
    reset();
    lc(&x, "kickstart", "-p", SYS, NULL);
    p = (pid_t)atol(x.out);
    reply_shape("unheld start", &x);
    lc(&y, "kickstart", "-p", SYS, NULL);
    reply_shape("unheld kickstart -p over the running launch", &y);
    tally("unheld no-op names the pid: %s", (pid_t)atol(y.out) == p ? "yes" : "no");
    lc(&c, "kill", "SIGKILL", SYS, NULL);
    if (i == 0) say("---- first pass ordering: tb->ta bracket %lld us ----", (long long)(diff_ns(ta, tb) / 1000));
}

static void spin_us(long us) {
    uint64_t t0 = mach_absolute_time();
    while (ns_of(mach_absolute_time() - t0) < us * 1000) {}
}

static void race(int i, long max_us, int withprint) {
    struct cmd c, d, k2, pr;
    long runs0 = -1, runs1 = -1, pid1 = -1;
    char state1[64] = "";
    reset();
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    if (c.code) tally("H0 debug exit %d", c.code);
    if (withprint) {
        lc(&pr, "print", SYS, NULL);
        runs0 = field(pr.out, "runs = ");
    }
    struct kp x;
    if (kick_spawn(&x, 0)) {
        anom("race %d: spawn failed", i);
        return;
    }
    spin_us(rand() % (max_us + 1));
    uint64_t tb = mach_absolute_time();
    lc(&d, "debug", SYS, "--start-suspended", NULL);
    uint64_t ta = mach_absolute_time();
    if (withprint) {
        lc(&pr, "print", SYS, NULL);
        runs1 = field(pr.out, "runs = ");
        pid1 = field(pr.out, "pid = ");
        const char *s = strstr(pr.out, "\n\tstate = ");
        snprintf(state1, sizeof state1, "%s", s ? s + 10 : "(none)");
        char *nl = strchr(state1, '\n');
        if (nl) *nl = 0;
    }
    lc(&k2, "kickstart", "-p", SYS, NULL);
    pid_t q = (pid_t)atol(k2.out);
    kick_read_pid(&x);
    pid_t p = x.pid;
    struct cmd xc;
    kick_finish(&x, &xc);
    int e = d.code;
    if (e != 0) {
        /* Our debug found the old hold pending, so a launch after it took that hold: nothing older to tell apart. */
        tally("our debug exit %d (the older hold was still pending)", e);
        return;
    }
    uint64_t st = 0;
    const char *where = "unreadable";
    if (p > 0 && start_abstime(p, &st) == 0) where = st > ta ? "AFTER ta" : (st >= tb ? "INSIDE [tb,ta]" : "before tb");
    int s1 = 0;
    int stat = p > 0 ? sys_stat_of(p, &s1) : -1;
    tally("our debug exit 0; older launch (took the old hold): start time %s; K2 named it: %s; reads %s", where, q == p ? "yes" : "no", sname(stat));
    if (!strcmp(where, "AFTER ta") || !strcmp(where, "INSIDE [tb,ta]")) {
        int64_t d1 = (p > 0 && st) ? diff_ns(st, tb) : 0, d2 = (p > 0 && st) ? diff_ns(st, ta) : 0;
        anom("race %d: older launch start %s: start-tb %lld us, start-ta %lld us, bracket %lld us", i, where, (long long)(d1 / 1000), (long long)(d2 / 1000),
             (long long)(diff_ns(ta, tb) / 1000));
    }
    if (withprint)
        tally("print after our debug: state [%s], pid attached %s, runs increase since before %ld (start %s)", state1, pid1 > 0 ? "yes" : "no",
              runs1 - runs0, where);
    lc(&c, "kill", "SIGKILL", SYS, NULL);
    if (p > 0) gone(p);
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    tally("after it ended, debug exit %d (37 = our fresh hold still pending)", c.code);
}

/* How long after `kickstart -p` names a held launch's pid does a `debug` still answer 37 (the hold still pending)? */
static void lag(int i) {
    struct cmd c, x;
    reset();
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    lc(&x, "kickstart", "-p", SYS, NULL);
    pid_t p = (pid_t)atol(x.out);
    int e1 = 0;
    int stat = sys_stat_of(p, &e1);
    uint64_t t0 = mach_absolute_time();
    int tries = 0;
    do {
        lc(&c, "debug", SYS, "--start-suspended", NULL);
        tries++;
    } while (c.code == 37 && tries < 2000);
    int64_t us = ns_of(mach_absolute_time() - t0) / 1000;
    tally("debug after kickstart -p named a %s launch: answered 37 %d time(s) before 0 (last exit %d)", sname(stat), tries - 1, c.code);
    if (tries > 1) anom("lag %d: %d tries, %lld us", i, tries, (long long)us);
    lc(&c, "kill", "SIGKILL", SYS, NULL);
    gone(p);
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    tally("after the launch ended, debug exit %d (37 = the hold armed after it is still pending)", c.code);
}

/* Does a `kickstart -p` over a running held launch lose the fresh hold pending behind it? Alternating passes with and
 * without it: after the launch ends, `debug` answering 37 means the hold is still pending, 0 that it is gone. */
static void noop(int i) {
    int with = i % 2;
    struct cmd c, x, y;
    reset();
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    lc(&x, "kickstart", "-p", SYS, NULL);
    pid_t p = (pid_t)atol(x.out);
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    int armed = c.code;
    if (with) lc(&y, "kickstart", "-p", SYS, NULL);
    lc(&c, "kill", "SIGKILL", SYS, NULL);
    gone(p);
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    tally("%s a no-op kickstart -p: our debug after the older launch took the hold exit %d; after it ended, debug exit %d", with ? "WITH" : "without", armed,
          c.code);
}

/* ---- identity: can a pid be confirmed as the job's launch? (I1-I4) ---- */

#define LABEL2 "goetia.probe.runident2"
#define SYS2 "system/" LABEL2
#define PLIST2 "/Library/LaunchDaemons/" LABEL2 ".plist"

static void path_base(pid_t pid, char *buf, size_t cap) {
    char path[PROC_PIDPATHINFO_MAXSIZE];
    if (proc_pidpath(pid, path, sizeof path) > 0) {
        const char *b = strrchr(path, '/');
        snprintf(buf, cap, "%s", b ? b + 1 : path);
    } else
        snprintf(buf, cap, "(none)");
}

/* The resource coalition id of a pid by proc_pidinfo flavor PROC_PIDCOALITIONINFO (20), or -1. */
static long long coal_of(pid_t pid) {
    static const int sizes[] = {40, 16};
    for (int i = 0; i < 2; i++) {
        uint64_t b[8] = {0};
        int n = proc_pidinfo(pid, 20, 0, b, sizes[i]);
        if (n > 0) {
            tally("coalition read: struct size %d answered %d bytes", sizes[i], n);
            return (long long)b[0];
        }
    }
    tally("coalition read failed (errno %d)", errno);
    return -1;
}

/* The first number after the first "resource coalition" (else "coalition") in a `launchctl print` output, or -1. */
static long long print_coal(const char *out) {
    const char *p = strstr(out, "resource coalition");
    if (!p) p = strstr(out, "oalition");
    if (!p) return -1;
    while (*p && (*p < '0' || *p > '9')) p++;
    return *p ? strtoll(p, NULL, 10) : -1;
}

static const char *eq(long long a, long long b) { return (a < 0 || b < 0) ? "n/a" : (a == b ? "YES" : "no"); }

static void procinfo(const char *what, pid_t pid) {
    char pb[32], pa[32], s[16];
    struct cmd c;
    snprintf(s, sizeof s, "%d", pid);
    path_base(pid, pb, sizeof pb);
    int e = 0;
    int sb = bsd_stat_of(pid, &e);
    lc(&c, "procinfo", s, NULL);
    int sa = bsd_stat_of(pid, &e);
    path_base(pid, pa, sizeof pa);
    int lab = strstr(c.out, LABEL) || strstr(c.err, LABEL);
    if (tally("I1 procinfo of %s (path %s -> %s; state %s -> %s): exit %d, label named %s, stdout %s, stderr %s", what, pb, pa, sname(sb), sname(sa), c.code,
              lab ? "YES" : "no", c.out[0] ? "non-empty" : "empty", c.err[0] ? "non-empty" : "empty"))
        say("----- procinfo output, first of [%s, path %s -> %s, exit %d] -----\n%s----- stderr -----\n%s-----", what, pb, pa, c.code, c.out, c.err);
}

/* 1 = NOTE_EXEC for pid, 2 = its exit came first, 0 = nothing in 5 s (the probe's failure bound). */
static int wait_exec(pid_t pid) {
    for (;;) {
        struct ev f;
        if (!next_ev(&f, 5000)) return 0;
        if (f.pid != pid) continue;
        if (f.fflags & NOTE_EXEC) return 1;
        if (f.fflags & NOTE_EXIT) return 2;
    }
}

/* 1 = NOTE_EXIT for pid seen, 0 = nothing in 5 s. seq gets the events seen and the exit status. */
static int wait_exit(pid_t pid, char *seq, size_t cap) {
    seq[0] = 0;
    for (;;) {
        struct ev f;
        char one[48];
        if (!next_ev(&f, 5000)) {
            strncat(seq, "(none in 5s)", cap - strlen(seq) - 1);
            return 0;
        }
        if (f.pid != pid) continue;
        snprintf(one, sizeof one, "%s%s", seq[0] ? "," : "", evname(f.fflags));
        strncat(seq, one, cap - strlen(seq) - 1);
        if (f.fflags & NOTE_EXIT) {
            int st = (int)f.data;
            snprintf(one, sizeof one, "(%s %d)", WIFEXITED(st) ? "exit" : "signal", WIFEXITED(st) ? WEXITSTATUS(st) : WTERMSIG(st));
            strncat(seq, one, cap - strlen(seq) - 1);
            return 1;
        }
    }
}

static int O_fresh; /* ident: bootout and bootstrap before every pass, so each launch is the job's first */

/* I1 and I2: one launch of the job, read from the xpcproxy stage on. */
static void ident_pass(int i, int held, int early) {
    struct cmd c, pr;
    char pb[32], pa[32], pc[32];

    /* A pid no job owns, and one that no longer exists. */
    pid_t ch = fork();
    if (ch == 0) {
        execl("/bin/sleep", "sleep", "30", (char *)NULL);
        _exit(127);
    }
    procinfo("a pid no job owns (our own child)", ch);
    long long cc = coal_of(ch), own = coal_of(getpid());
    kill(ch, SIGKILL);
    reap(ch);
    procinfo("a pid that no longer exists (our reaped child)", ch);

    if (O_fresh) reset();
    if (held) {
        lc(&c, "debug", SYS, "--start-suspended", NULL);
        if (c.code) tally("debug exit %d", c.code);
    }
    struct kp k;
    if (kick_spawn(&k, 0)) {
        anom("pass %d: spawn kickstart failed", i);
        return;
    }
    kick_read_pid(&k);
    pid_t pid = k.pid;
    if (pid < 0) {
        kick_finish(&k, &c);
        anom("pass %d: no pid", i);
        return;
    }
    if (!early) kick_finish(&k, &c);

    /* The reads the design would make right after the reply. */
    path_base(pid, pb, sizeof pb);
    long long c0 = coal_of(pid);
    path_base(pid, pa, sizeof pa);
    /* print right after the reply: does it name this launch's coalition, and the pid, with no lag? */
    lc(&pr, "print", SYS, NULL);
    long long pcx = print_coal(pr.out);
    tally("I2 print right after kickstart -p (%s launch of the job): coalition named %s; == launch's %s; pid shown %s", O_fresh ? "first" : "later",
          pcx >= 0 ? "yes" : "no", eq(c0, pcx), field(pr.out, "pid = ") == pid ? "this one" : "not this one");
    procinfo(early ? "the launch, kickstart -p still running" : "the launch right after kickstart -p", pid);

    int rr = reg(pid);
    if (early) kick_finish(&k, &c);
    if (rr) {
        tally("register errno %d", rr);
        anom("pass %d: register pid %d: errno %d", i, pid, rr);
        return;
    }
    path_base(pid, pc, sizeof pc);
    if (!strcmp(pc, "xpcproxy")) {
        int ex = wait_exec(pid);
        tally("path xpcproxy after registering: waited for exec: %s", ex == 1 ? "exec" : (ex == 2 ? "exit first" : "nothing in 5 s"));
    }
    procinfo(held ? "the held launch after its exec" : "a running unheld launch after its exec", pid);
    long long c1 = coal_of(pid);
    lc(&pr, "print", SYS, NULL);
    long long pcr = print_coal(pr.out);
    if (i == 0) say("----- print while the launch runs -----\n%s", pr.out);

    lc(&c, "kill", held ? "SIGCONT" : "SIGKILL", SYS, NULL);
    char seq[160];
    if (!wait_exit(pid, seq, sizeof seq)) {
        anom("pass %d: no exit seen: %s", i, seq);
        lc(&c, "kill", "SIGKILL", SYS, NULL);
    }
    procinfo("the launch just after its exit", pid);

    long long pci = -1;
    do {
        lc(&pr, "print", SYS, NULL);
    } while (field(pr.out, "pid = ") == pid);
    pci = print_coal(pr.out);
    if (i == 0) say("----- print, job idle -----\n%s", pr.out);
    tally("I2 print names a coalition: idle %s, running %s", pci >= 0 ? "yes" : "no", pcr >= 0 ? "yes" : "no");
    tally("I2 launch coalition at stage %s->%s (before procinfo): read %s; == idle print %s; == running print %s", pb, pa, c0 >= 0 ? "ok" : "FAILED",
          eq(c0, pci), eq(c0, pcr));
    tally("I2 launch coalition after exec: read %s; == idle print %s; == running print %s; == the stage read %s", c1 >= 0 ? "ok" : "FAILED", eq(c1, pci),
          eq(c1, pcr), eq(c1, c0));
    tally("I2 no-job child coalition: read %s; == idle print %s; == launch's %s; == the probe's own %s", cc >= 0 ? "ok" : "FAILED", eq(cc, pci), eq(cc, c1),
          eq(cc, own));
}

static void run_ident(int held, int wrap, int early, int passes) {
    anomalies = 0;
    install(wrap, held ? "true" : "sleep", 0);
    for (int i = 0; i < passes; i++) {
        ident_pass(i, held, early);
        if (i % 100 == 99) say("... %d passes", i + 1);
    }
    char title[160];
    snprintf(title, sizeof title, "IDENT %s %s %s passes=%d", held ? "held" : "unheld", wrap ? "wrap(/bin/sh)" : "plain", early ? "early" : "late", passes);
    dump(title);
    uninstall();
}

/* Kickstart `sys`, wait for the program's exec as goetia would, return the pid (or -1). */
static pid_t launch_to_exec(const char *sys) {
    struct cmd x;
    char b[32];
    lc(&x, "kickstart", "-p", sys, NULL);
    pid_t p = (pid_t)atol(x.out);
    if (p <= 0) {
        tally("kickstart -p %s printed no pid (exit %d)", sys, x.code);
        return -1;
    }
    int rr = reg(p);
    if (rr) {
        tally("register errno %d", rr);
        return -1;
    }
    path_base(p, b, sizeof b);
    if (!strcmp(b, "xpcproxy")) wait_exec(p);
    return p;
}

static void stop_launch(const char *sys, pid_t p) {
    struct cmd c;
    char seq[160];
    lc(&c, "kill", "SIGKILL", sys, NULL);
    wait_exit(p, seq, sizeof seq);
}

/* I2: the coalition across launches, a bootout and bootstrap, and two jobs. */
static void run_coal(int passes) {
    anomalies = 0;
    struct cmd c, pr;
    install(0, "sleep", 0);
    lc(&c, "bootout", SYS2, NULL);
    write_plist_for(LABEL2, PLIST2, 0, "sleep", 0);
    bootstrap_plist(PLIST2);
    long long prev1 = -1, prev2 = -1;
    int rebooted = 0;
    long long own = coal_of(getpid());
    for (int i = 0; i < passes; i++) {
        lc(&pr, "print", SYS, NULL);
        long long idle1 = print_coal(pr.out);
        if (i == 0) say("----- print of job 1, never launched -----\n%s", pr.out);
        lc(&pr, "print", SYS2, NULL);
        long long idle2 = print_coal(pr.out);
        pid_t a = launch_to_exec(SYS);
        long long ca = a > 0 ? coal_of(a) : -1;
        lc(&pr, "print", SYS, NULL);
        long long pa = print_coal(pr.out);
        if (a > 0) stop_launch(SYS, a);
        pid_t b = launch_to_exec(SYS2);
        long long cb = b > 0 ? coal_of(b) : -1;
        lc(&pr, "print", SYS2, NULL);
        long long pb = print_coal(pr.out);
        if (b > 0) stop_launch(SYS2, b);
        tally("I2 job 1 launch coalition: == its running print %s; == its idle print %s; == the previous pass's launch %s%s", eq(ca, pa), eq(ca, idle1),
              eq(ca, prev1), rebooted ? " (a bootout+bootstrap between)" : "");
        tally("I2 job 2 launch coalition: == its running print %s; == its idle print %s; == the previous pass's launch %s", eq(cb, pb), eq(cb, idle2),
              eq(cb, prev2));
        tally("I2 two jobs: launch coalitions differ %s; print coalitions differ %s", (ca < 0 || cb < 0) ? "n/a" : (ca != cb ? "YES" : "no"),
              (pa < 0 || pb < 0) ? "n/a" : (pa != pb ? "YES" : "no"));
        tally("I2 job 1 launch coalition == the probe's own: %s", eq(ca, own));
        prev1 = ca;
        prev2 = cb;
        rebooted = 0;
        if (i % 5 == 4) {
            reset();
            rebooted = 1;
            lc(&pr, "print", SYS, NULL);
            tally("I2 job 1 print coalition after bootout+bootstrap == before: %s", eq(print_coal(pr.out), pa));
        }
        if (i % 100 == 99) say("... %d passes", i + 1);
    }
    dump("COAL");
    lc(&c, "kill", "SIGKILL", SYS2, NULL);
    lc(&c, "bootout", SYS2, NULL);
    unlink(PLIST2);
    uninstall();
}

/* I3: a held launch killed while still xpcproxy: is its hold consumed? */
static void killed_pass(int i) {
    int variant = i % 2;
    struct cmd c, d;
    struct ev s;
    char pb[32], pb2[32], seq[160];
    while (next_ev(&s, 0)) {}
    reset();
    lc(&c, "debug", SYS, "--start-suspended", NULL);
    if (c.code) tally("arming debug exit %d", c.code);
    struct kp k;
    if (kick_spawn(&k, 0)) return;
    kick_read_pid(&k);
    pid_t pid = k.pid;
    kick_finish(&k, &c);
    if (pid < 0) {
        tally("kickstart -p printed no pid");
        return;
    }
    int rr = reg(pid);
    path_base(pid, pb, sizeof pb);
    int kr = kill(pid, SIGKILL), kerrno = kr ? errno : 0;
    int exited = rr ? 0 : wait_exit(pid, seq, sizeof seq);
    if (rr) snprintf(seq, sizeof seq, "(not registered)");
    tally("I3 kill of a launch whose path read %s: register %s, kill errno %d, events %s", pb, rr ? "failed" : "ok", kerrno, seq);
    (void)exited;
    if (variant == 0) {
        lc(&d, "debug", SYS, "--start-suspended", NULL);
        tally("I3a launch killed at path %s, then debug: exit %d (37 = the hold is still pending)", pb, d.code);
        return;
    }
    /* Without a new debug: does the next launch start held? */
    pid_t p2 = launch_to_exec(SYS);
    if (p2 < 0) {
        tally("I3b launch killed at path %s, next launch: gone before registration (not held)", pb);
        return;
    }
    int e = 0;
    path_base(p2, pb2, sizeof pb2);
    tally("I3b launch killed at path %s, next launch, no new debug: reads %s (STOP = the hold was still pending)", pb, sname(bsd_stat_of(p2, &e)));
    stop_launch(SYS, p2);
}

/* I4: how fresh pids are handed out. */
static void run_pids(long n) {
    int prev = 0, mx = 0, mn = INT32_MAX, first[40], nf = 0, wrapped = 0, after[16], na = 0;
    long seq1 = 0, small = 0, mid = 0, big = 0, back = 0;
    int maxproc = 0;
    size_t ml = sizeof maxproc;
    if (!sysctlbyname("kern.maxproc", &maxproc, &ml, NULL, 0)) say("kern.maxproc = %d", maxproc);
    for (long i = 0; i < n; i++) {
        pid_t p = fork();
        if (p == 0) _exit(0);
        if (p < 0) {
            say("fork failed errno %d after %ld", errno, i);
            break;
        }
        reap(p);
        if (nf < 40) first[nf++] = p;
        if (p > mx) mx = p;
        if (p < mn) mn = p;
        if (prev) {
            long d = (long)p - prev;
            if (d == 1) seq1++;
            else if (d > 1 && d <= 10) small++;
            else if (d > 10 && d <= 100) mid++;
            else if (d > 100) big++;
            else {
                back++;
                if (back <= 5) say("step backward: %d -> %d (fork number %ld)", prev, p, i);
                wrapped = 1;
            }
        }
        if (wrapped && na < 16) after[na++] = p;
        prev = p;
    }
    say("I4 forks %ld: next pid is previous+1 in %ld, +2..10 in %ld, +11..100 in %ld, more than +100 in %ld, backward steps %ld", n, seq1, small, mid, big, back);
    say("I4 smallest pid seen %d, largest %d", mn, mx);
    printf("I4 first 40 pids:");
    for (int i = 0; i < nf; i++) printf(" %d", first[i]);
    printf("\nI4 the 16 pids from the first backward step:");
    for (int i = 0; i < na; i++) printf(" %d", after[i]);
    printf("\n");
    fflush(stdout);
}

/* ---- main ---- */

int main(int argc, char **argv) {
    if (geteuid() != 0 || argc < 3) {
        fprintf(stderr, "run as root; see the header\n");
        return 2;
    }
    mach_timebase_info(&tbi);
    kq = kqueue();
    if (kq < 0) {
        perror("kqueue");
        return 2;
    }
    srand(12345);
    const char *sc = argv[1];
    if (!strcmp(sc, "held") || !strcmp(sc, "unheld")) {
        if (argc < 6) return 2;
        sanity();
        run_passes(!strcmp(sc, "held"), !strcmp(argv[2], "wrap"), !strcmp(argv[3], "late"), !strcmp(argv[4], "spaced"), atoi(argv[5]),
                   argc > 6 ? atoi(argv[6]) : 0);
    } else if (!strcmp(sc, "reply")) {
        install(0, "sleep", 0);
        int n = atoi(argv[2]);
        for (int i = 0; i < n; i++) reply(i);
        dump("REPLY");
        uninstall();
    } else if (!strcmp(sc, "lag")) {
        install(0, "sleep", 0);
        int n = atoi(argv[2]);
        for (int i = 0; i < n; i++) lag(i);
        dump("LAG");
        uninstall();
    } else if (!strcmp(sc, "noop")) {
        install(0, "sleep", 0);
        int n = atoi(argv[2]);
        for (int i = 0; i < n; i++) noop(i);
        dump("NOOP");
        uninstall();
    } else if (!strcmp(sc, "ident")) { /* ident <held|unheld> <plain|wrap> <early|late> <passes> */
        if (argc < 6) return 2;
        O_fresh = argc > 6 && !strcmp(argv[6], "fresh");
        run_ident(!strcmp(argv[2], "held"), !strcmp(argv[3], "wrap"), !strcmp(argv[4], "early"), atoi(argv[5]));
    } else if (!strcmp(sc, "coal")) { /* coal <passes> */
        run_coal(atoi(argv[2]));
    } else if (!strcmp(sc, "killed")) { /* killed <plain|wrap> <passes> */
        if (argc < 4) return 2;
        anomalies = 0;
        install(!strcmp(argv[2], "wrap"), "true", 0);
        int n = atoi(argv[3]);
        for (int i = 0; i < n; i++) {
            killed_pass(i);
            if (i % 100 == 99) say("... %d passes", i + 1);
        }
        char t[64];
        snprintf(t, sizeof t, "KILLED %s passes=%d", argv[2], n);
        dump(t);
        uninstall();
    } else if (!strcmp(sc, "pids")) { /* pids <forks> */
        run_pids(atol(argv[2]));
    } else if (!strcmp(sc, "race")) {
        if (argc < 5) return 2;
        install(0, "sleep", 0);
        int n = atoi(argv[2]);
        for (int i = 0; i < n; i++) {
            race(i, atol(argv[3]), atoi(argv[4]));
            if (i % 100 == 99) say("... %d passes", i + 1);
        }
        char t[64];
        snprintf(t, sizeof t, "RACE max-delay %s us, withprint %s", argv[3], argv[4]);
        dump(t);
        uninstall();
    } else
        return 2;
    return 0;
}
