/* A launch is observable even when preparation passes unexpected arguments. */
#include <stdio.h>

int main(void) {
    FILE *marker = fopen("checker.launches", "a");
    return !marker || fputs("launch\n", marker) < 0 || fclose(marker);
}
