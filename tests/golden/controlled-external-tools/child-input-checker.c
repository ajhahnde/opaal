/* Observe the bytes a child reopens, including a change hidden by restoration. */
#include <stdio.h>
#include <string.h>

extern char **environ;

static int write_file(const char *path, const unsigned char *bytes, size_t length) {
    FILE *file = fopen(path, "wb");
    if (!file) return 0;
    int ok = fwrite(bytes, 1, length, file) == length;
    return fclose(file) == 0 && ok;
}

int main(int argc, char **argv) {
    if (argc != 7 || strcmp(argv[1], "--norc") || strcmp(argv[2], "--format=json1") ||
        strcmp(argv[3], "--shell=sh") || strcmp(argv[4], "--severity=style") ||
        strcmp(argv[5], "--") || argv[6][0] != '/' || !environ[0] || environ[1] ||
        strcmp(environ[0], "LC_ALL=C")) return 90;
    FILE *mode_file = fopen("checker.change", "r");
    int mode;
    if (!mode_file || fscanf(mode_file, "%d", &mode) != 1 || fclose(mode_file) ||
        mode < 0 || mode > 2) return 91;
    unsigned char before[65537];
    FILE *script = fopen(argv[6], "rb");
    if (!script) return 92;
    size_t length = fread(before, 1, sizeof(before), script);
    int failed = ferror(script);
    if (fclose(script) || failed || length == sizeof(before)) return 93;
    static const unsigned char changed[] = "#!/bin/sh\n# SENSITIVE_CHILD_INPUT\n";
    const unsigned char *seen = mode ? changed : before;
    size_t seen_length = mode ? sizeof(changed) - 1 : length;
    if (mode && !write_file(argv[6], changed, seen_length)) return 94;
    script = fopen(argv[6], "rb");
    unsigned char actual[65537];
    if (!script) return 95;
    size_t actual_length = fread(actual, 1, sizeof(actual), script);
    failed = ferror(script);
    if (fclose(script) || failed || actual_length != seen_length || memcmp(actual, seen, seen_length)) return 96;
    if (!write_file("checker.saw", actual, actual_length)) return 97;
    if (mode == 2 && !write_file(argv[6], before, length)) return 98;
    FILE *marker = fopen("checker.launches", "a");
    if (!marker || fputs("launch\n", marker) < 0 || fclose(marker)) return 99;
    if (fputs("{\"comments\":[]}", stdout) < 0 || fflush(stdout)) return 100;
    return 0;
}
