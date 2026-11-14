#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <unistd.h>

static int paused;
static char expected[PATH_MAX], ready[PATH_MAX];

__attribute__((constructor)) static void configure(void) {
    const char *value = getenv("ZUP_PROBE_LOCK");
    if (value) snprintf(expected, sizeof expected, "%s", value);
    value = getenv("ZUP_PROBE_READY");
    if (value) snprintf(ready, sizeof ready, "%s", value);
}

int flock(int fd, int operation) {
    int (*native)(int, int) = dlsym(RTLD_NEXT, "flock");
    if (!native) _exit(90);
    int result = native(fd, operation);
    if (!paused && !result && (operation & LOCK_EX) && expected[0] && ready[0]) {
        char descriptor[64], path[PATH_MAX];
        snprintf(descriptor, sizeof descriptor, "/proc/self/fd/%d", fd);
        ssize_t length = readlink(descriptor, path, sizeof path - 1);
        if (length >= 0) {
            path[length] = 0;
            if (!strcmp(path, expected)) {
                paused = 1;
                int marker = open(ready, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
                if (marker < 0) _exit(91);
                close(marker);
                raise(SIGSTOP);
            }
        }
    }
    return result;
}
