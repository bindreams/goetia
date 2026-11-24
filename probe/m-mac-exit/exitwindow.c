/* Claim under test (goetia queue, questions 1 and 2): what the kernel answers, by errno, to proc_pidinfo(PROC_PIDTBSDINFO),
 *   proc_pidpath and proc_pid_rusage for a process that (a) is in its exit window (P_WEXIT set, p_stat not yet SZOMB) and
 *   (b) has exited and is not reaped (a zombie); and whether kqueue EVFILT_PROC/NOTE_EXIT registration on a zombie is ESRCH.
 *
 * Build: cc -O1 -Wall -o exitwindow exitwindow.c
 * Run (as root and as a user; the target is never the prober's child):
 *   ./exitwindow window <passes> <D_us> <lead_us>
 *   ./exitwindow zombie <passes>
 * Each call is bracketed by a sysctl KERN_PROC_PID read before and after; the state class is only claimed when both
 * reads agree (the state only moves forward), so "exiting" means SRUN with P_WEXIT both before and after the call.
 */
#define _DARWIN_C_SOURCE
#include <errno.h>
#include <libproc.h>
#include <mach/mach_time.h>
#include <signal.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/mman.h>
#include <sys/proc_info.h>
#include <sys/resource.h>
#include <sys/sysctl.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef NOTE_EXITSTATUS
#define NOTE_EXITSTATUS 0x04000000
#endif
#define P_WEXIT_BIT 0x00002000

static void say(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
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
    if (ntal == 2048) return;
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

/* ---- the kernel's view of a pid by sysctl ---- */

enum { K_GONE, K_RUN, K_EXITING, K_ZOMBIE, K_OTHER, K_ERR };
static const char *kname[] = {"gone", "running", "exiting(SRUN+WEXIT)", "zombie(SZOMB)", "other-state", "sysctl-error"};

static int kclass(pid_t p) {
    int mib[4] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, (int)p};
    struct kinfo_proc kp;
    size_t sz;
    for (;;) {
        sz = sizeof kp;
        memset(&kp, 0, sizeof kp);
        if (sysctl(mib, 4, &kp, &sz, NULL, 0) == 0) break;
        if (errno == EINTR) continue;
        return K_ERR;
    }
    if (sz == 0) return K_GONE;
    int fl = kp.kp_proc.p_flag, st = kp.kp_proc.p_stat;
    if (st == SZOMB) return K_ZOMBIE;
    if (fl & P_WEXIT_BIT) return st == SRUN ? K_EXITING : K_OTHER;
    return (st == SRUN || st == SSLEEP) ? K_RUN : K_OTHER;
}

/* ---- the three reads: 0 on success, else errno ---- */

static const char *errname(int e) {
    switch (e) {
    case ESRCH: return "ESRCH";
    case EPERM: return "EPERM";
    case EACCES: return "EACCES";
    case EINVAL: return "EINVAL";
    case ENOMEM: return "ENOMEM";
    case EAGAIN: return "EAGAIN";
    case 0: return "errno-0";
    default: return "other";
    }
}

static void describe(char *out, size_t n, long ret, int e, int ok) {
    if (ok) snprintf(out, n, "ok(ret=%ld)", ret);
    else snprintf(out, n, "FAIL(ret=%ld, errno=%s%s%d)", ret, errname(e), strcmp(errname(e), "other") ? "=" : " ", e);
}

static void do_info(pid_t p, char *out, size_t n) {
    struct proc_bsdinfo b;
    errno = 0;
    int r = proc_pidinfo(p, PROC_PIDTBSDINFO, 0, &b, sizeof b);
    int e = errno;
    describe(out, n, r, e, r == (int)sizeof b);
}

static void do_path(pid_t p, char *out, size_t n) {
    char buf[PROC_PIDPATHINFO_MAXSIZE];
    errno = 0;
    int r = proc_pidpath(p, buf, sizeof buf);
    int e = errno;
    describe(out, n, r, e, r > 0);
}

static void do_rusage(pid_t p, char *out, size_t n) {
    rusage_info_v0 u;
    errno = 0;
    int r = proc_pid_rusage(p, RUSAGE_INFO_V0, (rusage_info_t *)&u);
    int e = errno;
    describe(out, n, r, e, r == 0);
}

typedef void (*readfn)(pid_t, char *, size_t);
static const struct {
    const char *name;
    readfn fn;
} reads[] = {{"proc_pidinfo(PROC_PIDTBSDINFO)", do_info}, {"proc_pidpath", do_path}, {"proc_pid_rusage(V0)", do_rusage}};

/* Bracket one read with two sysctl reads; record under the state class only when both agree. */
static int bracketed(pid_t pid, int which, const char *what) {
    int s0 = kclass(pid);
    char res[96];
    reads[which].fn(pid, res, sizeof res);
    int s1 = kclass(pid);
    (void)what;
    char line[200];
    snprintf(line, sizeof line, "%-22s %-32s %s", s0 == s1 ? kname[s0] : "state moved during the call", reads[which].name, res);
    tally("%s", line);
    return s1;
}

/* ---- the helper: parent of the target ---- */

struct slot {
    uint64_t start;
};
static struct slot *slot;
static int cmdp[2], pidp[2], repp[2];
static int64_t D_ticks;
static mach_timebase_info_data_t tbi;
static int64_t ticks_of(int64_t ns) { return (int64_t)((__int128)ns * tbi.denom / tbi.numer); }

static void spin_until(int64_t aim) {
    while ((int64_t)mach_absolute_time() < aim) {}
}

static void helper(int hold) {
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
            spin_until((int64_t)s + D_ticks);
            _exit(7);
        }
        write(pidp[1], &t, sizeof t);
        if (hold) {
            do n = read(cmdp[0], &c, 1);
            while (n < 0 && errno == EINTR);
        }
        int st = 0;
        while (waitpid(t, &st, 0) < 0 && errno == EINTR) {}
        write(repp[1], &st, sizeof st);
    }
}

static void helper_start(int hold) {
    slot = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANON, -1, 0);
    if (slot == MAP_FAILED || pipe(cmdp) || pipe(pidp) || pipe(repp)) {
        perror("setup");
        exit(2);
    }
    pid_t h = fork();
    if (h < 0) {
        perror("fork");
        exit(2);
    }
    if (h == 0) helper(hold);
    close(cmdp[0]);
    close(pidp[1]);
    close(repp[1]);
}

static pid_t begin_pass(uint64_t *start) {
    __atomic_store_n(&slot->start, 0, __ATOMIC_RELEASE);
    char c = 'g';
    if (write(cmdp[1], &c, 1) != 1) {
        perror("write");
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

/* ---- mode window ---- */

static void run_window(int passes, int64_t d_us, int64_t lead_us) {
    D_ticks = ticks_of(d_us * 1000);
    helper_start(0);
    for (int i = 0; i < passes; i++) {
        uint64_t s;
        pid_t pid = begin_pass(&s);
        spin_until((int64_t)s + D_ticks - ticks_of(lead_us * 1000));
        for (;;) {
            int last = 0;
            for (int w = 0; w < 3; w++) last = bracketed(pid, w, "");
            if (last == K_GONE) break;
        }
        wait_reaped();
    }
    char title[128];
    snprintf(title, sizeof title, "window passes=%d D=%lldus lead=%lldus (state class, read, answer)", passes, (long long)d_us, (long long)lead_us);
    dump(title);
}

/* ---- mode zombie ---- */

static int add_exit(int kq, pid_t pid, int es) {
    struct kevent ch;
    EV_SET(&ch, (uintptr_t)pid, EVFILT_PROC, EV_ADD | EV_CLEAR, NOTE_EXIT | (es ? NOTE_EXITSTATUS : 0), 0, NULL);
    for (;;) {
        if (kevent(kq, &ch, 1, NULL, 0, NULL) == 0) return 0;
        if (errno == EINTR) continue;
        return errno;
    }
}

static void run_zombie(int passes) {
    D_ticks = 0;
    helper_start(1);
    for (int i = 0; i < passes; i++) {
        uint64_t s;
        pid_t pid = begin_pass(&s);
        while (kclass(pid) != K_ZOMBIE) {} /* the helper holds the reap, so this ends once the target has exited */
        for (int rep = 0; rep < 3; rep++)
            for (int w = 0; w < 3; w++) bracketed(pid, w, "");
        for (int es = 0; es < 2; es++) {
            int kq = kqueue();
            int s0 = kclass(pid);
            int e = add_exit(kq, pid, es);
            int s1 = kclass(pid);
            tally("%-22s kqueue EV_ADD NOTE_EXIT%s: %s%s", s0 == s1 ? kname[s0] : "state moved during the call", es ? "|NOTE_EXITSTATUS" : "",
                  e == 0 ? "ok" : errname(e), (e && !strcmp(errname(e), "other")) ? " (other errno)" : "");
            close(kq);
        }
        char c = 'r';
        write(cmdp[1], &c, 1);
        wait_reaped();
    }
    char title[128];
    snprintf(title, sizeof title, "zombie passes=%d (state class, read, answer; 9 reads and 2 registrations per pass)", passes);
    dump(title);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    mach_timebase_info(&tbi);
    say("uid %d; offsetof p_flag=%zu p_stat=%zu in kinfo_proc", (int)getuid(), offsetof(struct kinfo_proc, kp_proc.p_flag),
        offsetof(struct kinfo_proc, kp_proc.p_stat));
    if (argc == 5 && !strcmp(argv[1], "window")) run_window(atoi(argv[2]), atoll(argv[3]), atoll(argv[4]));
    else if (argc == 3 && !strcmp(argv[1], "zombie")) run_zombie(atoi(argv[2]));
    else {
        fprintf(stderr, "usage: see the top of the file\n");
        return 2;
    }
    return 0;
}
