/* iOS 9 compatibility shims: std references iOS10+ libc symbols absent on 9.3.6 */
typedef long time_t;
typedef int clockid_t;
struct timespec { time_t tv_sec; long tv_nsec; };
struct timeval  { time_t tv_sec; int tv_usec; };
int gettimeofday(struct timeval *, void *);
int rename(const char *, const char *);
extern int errno;
#define ENOSYS 78

int clock_gettime(clockid_t clk, struct timespec *ts) {
    (void)clk; struct timeval tv;
    if (gettimeofday(&tv, 0) != 0) return -1;
    ts->tv_sec = tv.tv_sec; ts->tv_nsec = (long)tv.tv_usec * 1000;
    return 0;
}
int fclonefileat(int a, int b, const char *c, unsigned int d) {
    (void)a;(void)b;(void)c;(void)d; errno = ENOSYS; return -1;
}
int renamex_np(const char *from, const char *to, unsigned int flags) {
    (void)flags; return rename(from, to);
}
int sysdir_start_search_path_enumeration(unsigned long a, unsigned long b) {
    (void)a;(void)b; return 0; /* 0 = no more entries */
}
int sysdir_get_next_search_path_enumeration(int state, char *path) {
    (void)state; if (path) path[0] = 0; return 0;
}
