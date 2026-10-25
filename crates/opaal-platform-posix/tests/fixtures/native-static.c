/* Freestanding native Linux observer: no static libc installation is required. */
#if defined(__x86_64__)
#define SYS_WRITE 1
#define SYS_EXIT 60
#define SYS_FCNTL 72
static long syscall3(long n, long a, long b, long c) {
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return result;
}
#define START_OBSERVER ".global _start\n_start:\nmov %rsp, %rdi\nand $-16, %rsp\ncall observe\n"
#elif defined(__aarch64__)
#define SYS_WRITE 64
#define SYS_EXIT 93
#define SYS_FCNTL 25
static long syscall3(long n, long a, long b, long c) {
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    register long x8 __asm__("x8") = n;
    __asm__ volatile("svc 0" : "+r"(x0), "+r"(x8) : "r"(x1), "r"(x2) : "cc", "memory");
    return x0;
}
#define START_OBSERVER ".global _start\n_start:\nmov x0, sp\nbl observe\n"
#else
#error "native-static.c supports only x86_64 and AArch64 Linux"
#endif

__attribute__((used, noreturn)) static void observe(long *stack) {
    char **argv = (char **)(stack + 1);
    for (long i = 0; i < stack[0]; i++) {
        long length = 0;
        while (argv[i][length]) length++;
        if (syscall3(SYS_WRITE, 1, (long)argv[i], length) != length) syscall3(SYS_EXIT, 33, 0, 0);
        syscall3(SYS_WRITE, 1, (long)"\n", 1);
    }
    for (long fd = 3; fd < 1024; fd++) {
        if (syscall3(SYS_FCNTL, fd, 1, 0) != -9) syscall3(SYS_EXIT, 32, 0, 0);
    }
    syscall3(SYS_EXIT, 0, 0, 0);
    __builtin_unreachable();
}
__asm__(START_OBSERVER);
