/* AudioQueue PCM player for iOS 9 armv7. No SDK headers; declare the ABI by hand. */
typedef signed int   OSStatus;
typedef unsigned int UInt32;
typedef signed int   SInt32;
typedef double       Float64;
typedef void* AudioQueueRef;
typedef void* CFRunLoopRef;
typedef void* CFStringRef;

typedef struct AudioStreamBasicDescription {
    Float64 mSampleRate;
    UInt32  mFormatID;
    UInt32  mFormatFlags;
    UInt32  mBytesPerPacket;
    UInt32  mFramesPerPacket;
    UInt32  mBytesPerFrame;
    UInt32  mChannelsPerFrame;
    UInt32  mBitsPerChannel;
    UInt32  mReserved;
} AudioStreamBasicDescription;

typedef struct AudioQueueBuffer {
    UInt32 mAudioDataBytesCapacity;
    void*  mAudioData;
    UInt32 mAudioDataByteSize;
    void*  mUserData;
    UInt32 mPacketDescriptionCapacity;
    void*  mPacketDescriptions;
    UInt32 mPacketDescriptionCount;
} AudioQueueBuffer;
typedef AudioQueueBuffer* AudioQueueBufferRef;

typedef void (*AudioQueueOutputCallback)(void*, AudioQueueRef, AudioQueueBufferRef);

extern OSStatus AudioQueueNewOutput(const AudioStreamBasicDescription*, AudioQueueOutputCallback,
    void*, CFRunLoopRef, CFStringRef, UInt32, AudioQueueRef*);
extern OSStatus AudioQueueAllocateBuffer(AudioQueueRef, UInt32, AudioQueueBufferRef*);
extern OSStatus AudioQueueEnqueueBuffer(AudioQueueRef, AudioQueueBufferRef, UInt32, const void*);
extern OSStatus AudioQueueStart(AudioQueueRef, const void*);
extern OSStatus AudioQueueStop(AudioQueueRef, int);
extern OSStatus AudioQueueDispose(AudioQueueRef, int);

/* pthread (libSystem) */
typedef unsigned long pthread_t;
typedef struct { long sig; char opaque[56]; } pthread_mutex_t;
typedef struct { long sig; char opaque[40]; } pthread_cond_t;
extern int pthread_mutex_init(pthread_mutex_t*, const void*);
extern int pthread_mutex_lock(pthread_mutex_t*);
extern int pthread_mutex_unlock(pthread_mutex_t*);
extern int pthread_cond_init(pthread_cond_t*, const void*);
extern int pthread_cond_wait(pthread_cond_t*, pthread_mutex_t*);
extern int pthread_cond_signal(pthread_cond_t*);
void* memcpy(void*, const void*, unsigned long);
int printf(const char*, ...);

#define NBUF 6
#define BUFBYTES 16384  /* 4096 stereo s16 frames, ~93 ms each */

enum { B_FREE = 0, B_FILLING = 1, B_QUEUED = 2 };

typedef struct {
    AudioQueueRef q;
    AudioQueueBufferRef bufs[NBUF];
    int state[NBUF];
    int started;
    int flushing;
    pthread_mutex_t m;
    pthread_cond_t c;
} Player;

static int slot(Player* p, AudioQueueBufferRef b) {
    for (int i = 0; i < NBUF; i++) if (p->bufs[i] == b) return i;
    return -1;
}

static int nfree(Player* p) {
    int n = 0;
    for (int i = 0; i < NBUF; i++) if (p->state[i] == B_FREE) n++;
    return n;
}

static void cb(void* ud, AudioQueueRef q, AudioQueueBufferRef b) {
    Player* p = (Player*)ud;
    pthread_mutex_lock(&p->m);
    int i = slot(p, b);
    if (i >= 0 && p->state[i] == B_QUEUED) p->state[i] = B_FREE;
    pthread_cond_signal(&p->c);
    pthread_mutex_unlock(&p->m);
}

extern int pthread_cond_broadcast(pthread_cond_t*);

void* aq_new(int sample_rate, int channels) {
    static Player P;
    Player* p = &P;
    p->started = 0; p->flushing = 0;
    pthread_mutex_init(&p->m, 0);
    pthread_cond_init(&p->c, 0);
    AudioStreamBasicDescription f;
    for (unsigned i=0;i<sizeof(f);i++) ((char*)&f)[i]=0;
    f.mSampleRate = (Float64)sample_rate;
    f.mFormatID = 0x6C70636D;            /* 'lpcm' */
    f.mFormatFlags = 0x4 | 0x8;          /* signed int | packed */
    f.mChannelsPerFrame = channels;
    f.mBitsPerChannel = 16;
    f.mBytesPerFrame = 2 * channels;
    f.mFramesPerPacket = 1;
    f.mBytesPerPacket = 2 * channels;
    OSStatus st = AudioQueueNewOutput(&f, cb, p, 0, 0, 0, &p->q);
    if (st) { printf("[aq] NewOutput err %d\n", (int)st); return 0; }
    for (int i=0;i<NBUF;i++) {
        if (AudioQueueAllocateBuffer(p->q, BUFBYTES, &p->bufs[i])) { printf("[aq] alloc err\n"); return 0; }
        p->state[i] = B_FREE;
    }
    return p;
}

/* Copy interleaved s16 into queue buffers, blocking for a free buffer. Enqueue happens under the
   lock so a concurrent aq_flush can never miss or double-free a buffer. */
void aq_write(void* h, const unsigned char* data, int len) {
    Player* p = (Player*)h;
    int off = 0;
    while (off < len) {
        pthread_mutex_lock(&p->m);
        while (p->flushing || nfree(p) == 0) pthread_cond_wait(&p->c, &p->m);
        int i = 0; while (p->state[i] != B_FREE) i++;
        p->state[i] = B_FILLING;
        pthread_mutex_unlock(&p->m);

        AudioQueueBufferRef b = p->bufs[i];
        int n = len - off; if (n > BUFBYTES) n = BUFBYTES;
        memcpy(b->mAudioData, data + off, n);
        b->mAudioDataByteSize = n;
        off += n;

        pthread_mutex_lock(&p->m);
        if (p->flushing) { p->state[i] = B_FREE; pthread_mutex_unlock(&p->m); continue; }
        p->state[i] = B_QUEUED;
        AudioQueueEnqueueBuffer(p->q, b, 0, 0);
        if (!p->started && NBUF - nfree(p) >= 3) { AudioQueueStart(p->q, 0); p->started = 1; }
        pthread_mutex_unlock(&p->m);
    }
}

void aq_start(void* h) { Player* p=(Player*)h; if(!p->started){ AudioQueueStart(p->q,0); p->started=1; } }

extern OSStatus AudioQueuePause(AudioQueueRef);
/* pause keeps buffered audio; resume continues where it stopped */
void aq_pause(void* h) { Player* p=(Player*)h; if (p && p->started) AudioQueuePause(p->q); }
void aq_resume(void* h) { Player* p=(Player*)h; if (p && p->started) AudioQueueStart(p->q, 0); }

extern OSStatus AudioQueueReset(AudioQueueRef);
/* Drop queued audio immediately (track change / seek). iOS 9 does not reliably call the output
   callback for buffers discarded by Reset, so reclaim every queued buffer ourselves. */
void aq_flush(void* h) {
    Player* p = (Player*)h;
    if (!p) return;
    pthread_mutex_lock(&p->m);
    p->flushing = 1;
    pthread_mutex_unlock(&p->m);
    if (p->started) AudioQueueReset(p->q);
    pthread_mutex_lock(&p->m);
    for (int i = 0; i < NBUF; i++) if (p->state[i] == B_QUEUED) p->state[i] = B_FREE;
    p->flushing = 0;
    pthread_cond_broadcast(&p->c);
    pthread_mutex_unlock(&p->m);
}
