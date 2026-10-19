/* Synthetic acquisition observer, deliberately separate from real ShellCheck. */
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

extern char **environ;

int main(int argc, char **argv) {
    if (argc != 7 || strcmp(argv[1], "--norc") || strcmp(argv[2], "--format=json1") ||
        strcmp(argv[3], "--shell=sh") || strcmp(argv[4], "--severity=style") ||
        strcmp(argv[5], "--") || argv[6][0] != '/' ||
        !environ[0] || environ[1] || strcmp(environ[0], "LC_ALL=C")) return 90;
    FILE *marker = fopen("checker.launches", "a");
    if (!marker || fputs("launch\n", marker) < 0 || fclose(marker)) return 91;
    FILE *output = fopen("checker.stdout", "rb");
    if (!output) return 92;
    int byte;
    while ((byte = fgetc(output)) != EOF) if (putchar(byte) == EOF) return 93;
    if (ferror(output) || fclose(output) || fflush(stdout)) return 94;
    if (fputs("SENSITIVE_CHECKER_STDERR_CANARY\033[31m\n", stderr) < 0) return 95;
    FILE *status = fopen("checker.code", "r");
    int code;
    if (!status || fscanf(status, "%d", &code) != 1 || fclose(status)) return 96;
    if (code < 0) {
        raise(-code);
        return 97;
    }
    return code;
}
