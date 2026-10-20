/* Spotipad daemon entry: spotipad [data_dir] [port] */
int printf(const char*, ...);
int atoi(const char*);
int getpid(void);
extern int memorystatus_control(unsigned int command, int pid, unsigned int flags, void* buffer, unsigned long buffersize);
extern int spotipad_main(const char* dir, int port);

#define MEMORYSTATUS_CMD_SET_PRIORITY_PROPERTIES   2
#define MEMORYSTATUS_CMD_SET_JETSAM_HIGH_WATER_MARK 5
typedef struct { int priority; unsigned long long user_data; } memorystatus_priority_properties_t;

int main(int argc, char** argv) {
    const char* dir = argc > 1 ? argv[1] : "/var/root/spotipad";
    int port = argc > 2 ? atoi(argv[2]) : 8888;
    /* launchd daemons on iOS 9 get a tiny per-process jetsam limit; lift it (-1 = none) and put
       the daemon in the audio band so it is not reaped while playing. Needs root. */
    int pid = getpid();
    int r1 = memorystatus_control(MEMORYSTATUS_CMD_SET_JETSAM_HIGH_WATER_MARK, pid, (unsigned int)-1, 0, 0);
    memorystatus_priority_properties_t pp = { 12, 0 };
    int r2 = memorystatus_control(MEMORYSTATUS_CMD_SET_PRIORITY_PROPERTIES, pid, 0, &pp, sizeof(pp));
    printf("[spotipad] jetsam limit=%d priority=%d\n", r1, r2);
    int r = spotipad_main(dir, port);
    printf("[spotipad] exit %d\n", r);
    return r == 0 ? 0 : 1;
}
