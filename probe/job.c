/*
 * The program launchd runs for every probe row. Throwaway measurement code.
 *
 * Usage: job STATUS CTRL N1 N2 [bits path]...
 *
 * Records fds 0-2 before anything else runs (C, not Rust: std reopens closed
 * fds 0-2 as /dev/null before main). Exit codes >= 200 are job-internal
 * failures (200 + step), never launchd's outcome.
 */
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static char line[PIPE_BUF + 1];
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
	int getfd[3], getfl[3], fd_errno[3], st_errno[3];
	struct stat st[3];
	for (int fd = 0; fd < 3; fd++) {
		errno = 0;
		getfd[fd] = fcntl(fd, F_GETFD);
		getfl[fd] = fcntl(fd, F_GETFL);
		fd_errno[fd] = getfd[fd] < 0 ? errno : 0;
		st_errno[fd] = fstat(fd, &st[fd]) == 0 ? 0 : errno;
	}
	signal(SIGPIPE, SIG_IGN);

	if (argc < 5 || (argc - 5) % 2 != 0) {
		_exit(201);
	}
	const char *status_path = argv[1], *ctrl_path = argv[2];

	/* Step 2: one nonce per stream. Results are data. */
	char buf[256];
	int n1 = snprintf(buf, sizeof buf, "%s\n", argv[3]);
	ssize_t w1 = write(1, buf, (size_t)n1);
	int w1e = w1 < 0 ? errno : 0;
	int n2 = snprintf(buf, sizeof buf, "%s\n", argv[4]);
	ssize_t w2 = write(2, buf, (size_t)n2);
	int w2e = w2 < 0 ? errno : 0;

	/* Step 3: where launchd left us. */
	char cwd[PATH_MAX];
	int cwd_errno = getcwd(cwd, sizeof cwd) ? 0 : errno;
	struct stat dot;
	int dot_errno = stat(".", &dot) == 0 ? 0 : errno;

	add("ready pid=%d", (int)getpid());
	for (int fd = 0; fd < 3; fd++) {
		if (fd_errno[fd]) {
			add(" fd%d=err:%d", fd, fd_errno[fd]);
		} else if (st_errno[fd]) {
			add(" fd%d=%d,%d,staterr:%d", fd, getfd[fd], getfl[fd], st_errno[fd]);
		} else {
			add(" fd%d=%d,%d,%llu,%llu,%o,%llu", fd, getfd[fd], getfl[fd],
			    (unsigned long long)st[fd].st_dev, (unsigned long long)st[fd].st_ino,
			    (unsigned)st[fd].st_mode, (unsigned long long)st[fd].st_rdev);
		}
	}
	add(" w1=%zd,%d w2=%zd,%d", w1, w1e, w2, w2e);
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
