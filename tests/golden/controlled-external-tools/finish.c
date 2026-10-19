#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    FILE *marker = fopen("child.pid", "wx");
    if (!marker) return 31;
    if (fprintf(marker, "%ld\n", (long)getpid()) < 0 || fclose(marker)) return 32;
    if (access("request-cancel", F_OK) == 0) {
        pid_t parent = getppid();
        /* Keep the injected kill syscall exclusive to the supervisor. */
        if (syscall(SYS_tgkill, parent, parent, SIGINT)) return 33;
        for (;;) pause();
    }
    return 0;
}
