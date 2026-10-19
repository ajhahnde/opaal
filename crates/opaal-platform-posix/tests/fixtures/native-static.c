/* Freestanding x86_64 observer: no static libc installation is required. */
static long syscall3(long n, long a, long b, long c) {
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return result;
}
__attribute__((used, noreturn)) static void observe(long *stack) {
    char **argv = (char **)(stack + 1);
    for (long i = 0; i < stack[0]; i++) {
        long length = 0;
        while (argv[i][length]) length++;
        if (syscall3(1, 1, (long)argv[i], length) != length) syscall3(60, 33, 0, 0);
        syscall3(1, 1, (long)"\n", 1);
    }
    for (long fd = 3; fd < 1024; fd++) {
        if (syscall3(72, fd, 1, 0) != -9) syscall3(60, 32, 0, 0);
    }
    syscall3(60, 0, 0, 0);
    __builtin_unreachable();
}
__asm__(".global _start\n_start:\nmov %rsp, %rdi\nand $-16, %rsp\ncall observe\n");
