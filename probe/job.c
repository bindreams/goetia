/*
 * The program launchd runs for every probe row. Throwaway measurement code.
 *
 * Usage: job [--nonblock-stdio] STATUS CTRL N1 N2 [bits path]...
 *
 * Records fds 0-15 before anything else runs (C, not Rust: std reopens closed
 * fds 0-2 as /dev/null before main). After writing N1 to fd 1 it records the
 * file offsets of fd 2 and fd 1 (Q2: equal offsets on a regular file mean one
 * shared open file description). Records the public getiopolicy_np values.
 * With --nonblock-stdio, fds 1 and 2 get O_NONBLOCK (F_SETFL) before the first write; the
 * F_SETFL results and the flags afterwards are reported (pass 2, M1: a pty master with no
 * replica open).
 * Exit codes >= 200 are job-internal failures (200 + step), never launchd's
 * outcome.
 */
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <unistd.h>

#define NFDS 16

static char line[4096];
static size_t len;

static void add(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void add(const char *fmt, ...) {
	va_list ap;
	va_start(ap, fmt);
	int n = vsnprintf(line + len, sizeof line - len, fmt, ap);
	va_end(ap);
	if (n < 0 || (size_t)n >= sizeof line - len) {
		_exit(215); /* line would exceed PIPE_BUF */
	}
	len += (size_t)n;
}

int main(int argc, char **argv) {
	/* Step 1: fds 0-2 exactly as launchd left them. */
	int getfd[NFDS], getfl[NFDS], fd_errno[NFDS], st_errno[NFDS];
	struct stat st[NFDS];
	for (int fd = 0; fd < NFDS; fd++) {
		errno = 0;
		getfd[fd] = fcntl(fd, F_GETFD);
		getfl[fd] = fcntl(fd, F_GETFL);
		fd_errno[fd] = getfd[fd] < 0 ? errno : 0;
		st_errno[fd] = fstat(fd, &st[fd]) == 0 ? 0 : errno;
	}
	signal(SIGPIPE, SIG_IGN);

	int nonblock = 0;
	if (argc > 1 && strcmp(argv[1], "--nonblock-stdio") == 0) {
		nonblock = 1;
		argv[1] = argv[0];
		argv++;
		argc--;
	}

	if (argc < 5 || (argc - 5) % 2 != 0) {
		_exit(201);
	}
	const char *status_path = argv[1], *ctrl_path = argv[2];

	/* Step 2: one nonce per stream. Results are data. */
	int nb_ret[3] = {0, 0, 0}, nb_err[3] = {0, 0, 0}, nb_after[3] = {-1, -1, -1};
	if (nonblock) {
		for (int fd = 1; fd <= 2; fd++) {
			errno = 0;
			nb_ret[fd] = fcntl(fd, F_SETFL, (getfl[fd] < 0 ? 0 : getfl[fd]) | O_NONBLOCK);
			nb_err[fd] = nb_ret[fd] < 0 ? errno : 0;
			nb_after[fd] = fcntl(fd, F_GETFL);
		}
	}
	char buf[256];
	int n1 = snprintf(buf, sizeof buf, "%s\n", argv[3]);
	ssize_t w1 = write(1, buf, (size_t)n1);
	int w1e = w1 < 0 ? errno : 0;
	errno = 0;
	off_t l2 = lseek(2, 0, SEEK_CUR);
	int l2e = l2 < 0 ? errno : 0;
	errno = 0;
	off_t l1 = lseek(1, 0, SEEK_CUR);
	int l1e = l1 < 0 ? errno : 0;
	int n2 = snprintf(buf, sizeof buf, "%s\n", argv[4]);
	ssize_t w2 = write(2, buf, (size_t)n2);
	int w2e = w2 < 0 ? errno : 0;

	/* Step 3: where launchd left us. */
	char cwd[PATH_MAX];
	int cwd_errno = getcwd(cwd, sizeof cwd) ? 0 : errno;
	struct stat dot;
	int dot_errno = stat(".", &dot) == 0 ? 0 : errno;

	add("ready pid=%d", (int)getpid());
	for (int fd = 0; fd < NFDS; fd++) {
		if (fd_errno[fd]) {
			if (fd < 3) {
				add(" fd%d=err:%d", fd, fd_errno[fd]);
			}
		} else if (st_errno[fd]) {
			add(" fd%d=%d,%d,staterr:%d", fd, getfd[fd], getfl[fd], st_errno[fd]);
		} else {
			add(" fd%d=%d,%d,%llu,%llu,%o,%llu", fd, getfd[fd], getfl[fd],
			    (unsigned long long)st[fd].st_dev, (unsigned long long)st[fd].st_ino,
			    (unsigned)st[fd].st_mode, (unsigned long long)st[fd].st_rdev);
		}
	}
	add(" w1=%zd,%d w2=%zd,%d", w1, w1e, w2, w2e);
	add(" l1=%lld,%d l2=%lld,%d", (long long)l1, l1e, (long long)l2, l2e);
	add(" n1len=%d", n1);
	if (nonblock) {
		add(" nbset1=%d,%d nbset2=%d,%d flafter1=%d flafter2=%d", nb_ret[1], nb_err[1], nb_ret[2], nb_err[2],
		    nb_after[1], nb_after[2]);
	}
	add(" iopol=");
#define IOPOL(name, type) \
	add("%s%s:%d", iopol_first ? "" : ",", name, getiopolicy_np(type, IOPOL_SCOPE_PROCESS)), iopol_first = 0
	int iopol_first = 1;
	IOPOL("disk", IOPOL_TYPE_DISK);
#ifdef IOPOL_TYPE_VFS_ATIME_UPDATES
	IOPOL("atime", IOPOL_TYPE_VFS_ATIME_UPDATES);
#endif
#ifdef IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES
	IOPOL("dataless", IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES);
#endif
#ifdef IOPOL_TYPE_VFS_TRIGGER_RESOLVE
	IOPOL("trigger", IOPOL_TYPE_VFS_TRIGGER_RESOLVE);
#endif
#ifdef IOPOL_TYPE_VFS_CONTENT_PROTECTION
	IOPOL("contentprot", IOPOL_TYPE_VFS_CONTENT_PROTECTION);
#endif
#ifdef IOPOL_TYPE_VFS_IGNORE_PERMISSIONS
	IOPOL("ignoreperm", IOPOL_TYPE_VFS_IGNORE_PERMISSIONS);
#endif
#ifdef IOPOL_TYPE_VFS_SKIP_MTIME_UPDATE
	IOPOL("skipmtime", IOPOL_TYPE_VFS_SKIP_MTIME_UPDATE);
#endif
#ifdef IOPOL_TYPE_VFS_ALLOW_LOW_SPACE_WRITES
	IOPOL("lowspace", IOPOL_TYPE_VFS_ALLOW_LOW_SPACE_WRITES);
#endif
#ifdef IOPOL_TYPE_VFS_DISALLOW_RW_FOR_O_EVTONLY
	IOPOL("evtonly", IOPOL_TYPE_VFS_DISALLOW_RW_FOR_O_EVTONLY);
#endif
	if (dot_errno) {
		add(" dot=err:%d", dot_errno);
	} else {
		add(" dot=%llu,%llu", (unsigned long long)dot.st_dev, (unsigned long long)dot.st_ino);
	}

	/* Step 4: access() on exactly the credential launchd gave us. */
	add(" acc=");
	for (int i = 5; i + 1 < argc; i += 2) {
		int bits = atoi(argv[i]);
		int e = access(argv[i + 1], bits) == 0 ? 0 : errno;
		add("%s%d", i == 5 ? "" : ",", e);
	}
	if (cwd_errno) {
		add(" cwd=err:%d", cwd_errno);
	} else {
		add(" cwd=%s", cwd); /* last: may contain anything but a newline */
	}
	add("\n");

	/* Step 5: report. */
	int sfd = open(status_path, O_WRONLY | O_CLOEXEC);
	if (sfd < 0) {
		_exit(205);
	}
	if (write(sfd, line, len) != (ssize_t)len) {
		_exit(206);
	}
	close(sfd);

	/* Step 6: block until the probe releases us (EOF on CTRL). */
	int cfd = open(ctrl_path, O_RDONLY | O_CLOEXEC);
	if (cfd < 0) {
		_exit(207);
	}
	for (;;) {
		ssize_t r = read(cfd, buf, sizeof buf);
		if (r == 0) {
			break;
		}
		if (r < 0 && errno != EINTR) {
			_exit(208);
		}
	}
	_exit(0);
}
