#include <dirent.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    DIR *fds = opendir("/proc/self/fd");
    if (!fds) return 31;
    struct dirent *entry;
    while ((entry = readdir(fds))) {
        char path[512], target[512];
        snprintf(path, sizeof(path), "/proc/self/fd/%s", entry->d_name);
        ssize_t count = readlink(path, target, sizeof(target) - 1);
        if (count < 0) continue;
        target[count] = 0;
        if (strstr(target, "memfd:opaal-native")) return 32;
    }
    closedir(fds);
    if (argc > 1 && strcmp(argv[1], "--lifetime") == 0) {
        if (argc != 4) return 34;
        FILE *marker = fopen(argv[3], "a");
        if (!marker) return 35;
        fprintf(marker, "%ld\n", (long)getpid());
        if (fclose(marker)) return 36;
        if (strcmp(argv[2], "exit") == 0) return 17;
        if (strcmp(argv[2], "signal") == 0) {
            raise(SIGUSR1);
            return 37;
        }
        if (strcmp(argv[2], "output") == 0) {
            for (int i = 0; i < 1024; i++) {
                if (putchar('o') == EOF || fputc('e', stderr) == EOF) return 38;
            }
            return 0;
        }
        if (strncmp(argv[2], "stdout-", 7) == 0 || strncmp(argv[2], "stderr-", 7) == 0) {
            FILE *stream = argv[2][3] == 'o' ? stdout : stderr;
            size_t count = 8 * 1024 * 1024;
            if (strcmp(argv[2] + 7, "first-excess") == 0) count++;
            else if (strcmp(argv[2] + 7, "exact") != 0) return 46;
            char block[4096];
            memset(block, 'x', sizeof(block));
            while (count) {
                size_t chunk = count < sizeof(block) ? count : sizeof(block);
                if (fwrite(block, 1, chunk, stream) != chunk) return 47;
                count -= chunk;
            }
            return 0;
        }
        if (strcmp(argv[2], "context") == 0) {
            char cwd[4096];
            if (!getcwd(cwd, sizeof(cwd))) return 39;
            printf("%s\n%s\n", cwd, getenv("EXPLICIT"));
            return getenv("OPAAL_NATIVE_INHERIT_CANARY") ? 40 : 0;
        }
        if (signal(SIGTERM, SIG_IGN) == SIG_ERR) return 41;
        if (strcmp(argv[2], "descendant") == 0) {
            pid_t child = fork();
            if (child < 0) return 42;
            if (child == 0) {
                FILE *descendant = fopen(argv[3], "a");
                if (!descendant) _exit(43);
                fprintf(descendant, "%ld\n", (long)getpid());
                if (fclose(descendant)) _exit(44);
            }
        } else if (strcmp(argv[2], "sleep") != 0) return 45;
        for (;;) pause();
    }
    for (int i = 0; i < argc; i++) printf("%s\n", argv[i]);
    return 0;
}
