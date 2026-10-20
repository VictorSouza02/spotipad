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

#define NBUF 4
#define BUFBYTES 8192              /* 2048 stereo s16 frames, ~46 ms per AudioQueue buffer */
#define RING (1 << 19)             /* 512 KiB ring = ~3 s of 44.1 kHz stereo s16 */
#define PRIME (44100 * 4 / 2)      /* start playback once ~0.5 s is buffered */
#define REPRIME (44100 * 4 / 4)    /* after a flush (skip/seek) wait for ~0.25 s before feeding */

/* Pull model: the decoder thread pushes PCM into a large ring; the AudioQueue callback refills
   each returned buffer to full capacity from the ring. A decoder hiccup shorter than the ring
   depth is inaudible. A buffer is IDLE when we own it (not enqueued) and QUEUED when the queue does. */
enum { B_IDLE = 0, B_QUEUED = 1 };

typedef struct {
    AudioQueueRef q;
    AudioQueueBufferRef bufs[NBUF];
    int state[NBUF];
    int started, paused, flushing;
    int fresh;                     /* set by resume: an empty queue is expected, not an underrun */
    int priming;                   /* set by flush: hold buffers until REPRIME bytes are ready */
    unsigned char ring[RING];
    unsigned rd, wr;               /* free-running byte counters; fill = wr - rd */
    unsigned underruns;
    pthread_mutex_t m;
    pthread_cond_t c;
} Player;

static Player P;

static unsigned fill(Player* p) { return p->wr - p->rd; }

static int slot(Player* p, AudioQueueBufferRef b) {
    for (int i = 0; i < NBUF; i++) if (p->bufs[i] == b) return i;
    return -1;
}

/* Lock held. Copy up to one buffer from the ring and enqueue. Returns 0 if the ring is empty. */
static int refill(Player* p, int i) {
    unsigned n = fill(p);
    if (n == 0) return 0;
    if (n > BUFBYTES) n = BUFBYTES;
    AudioQueueBufferRef b = p->bufs[i];
    unsigned off = p->rd & (RING - 1), first = RING - off;
    if (first > n) first = n;
    memcpy(b->mAudioData, p->ring + off, first);
    if (n > first) memcpy((unsigned char*)b->mAudioData + first, p->ring, n - first);
    p->rd += n;
    b->mAudioDataByteSize = n;
    p->state[i] = B_QUEUED;
    AudioQueueEnqueueBuffer(p->q, b, 0, 0);
    return 1;
}

/* Lock held. Hand every idle buffer that the ring can fill to the queue. */
static void feed(Player* p) {
    for (int i = 0; i < NBUF; i++)
        if (p->state[i] == B_IDLE && !refill(p, i)) break;
}

static void cb(void* ud, AudioQueueRef q, AudioQueueBufferRef b) {
    Player* p = (Player*)ud;
    pthread_mutex_lock(&p->m);
    int i = slot(p, b);
    if (i >= 0 && p->state[i] == B_QUEUED) {
        p->state[i] = B_IDLE;
        if (!p->flushing) refill(p, i);
    }
    pthread_cond_signal(&p->c);
    pthread_mutex_unlock(&p->m);
}

extern int pthread_cond_broadcast(pthread_cond_t*);

void* aq_new(int sample_rate, int channels) {
    Player* p = &P;
    p->started = p->paused = p->flushing = p->fresh = p->priming = 0;
    p->rd = p->wr = 0; p->underruns = 0;
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
        p->state[i] = B_IDLE;
    }
    return p;
}

/* Copy interleaved s16 into the ring, blocking while it is full. */
void aq_write(void* h, const unsigned char* data, int len) {
    Player* p = (Player*)h;
    int off = 0;
    pthread_mutex_lock(&p->m);
    while (off < len) {
        while (p->flushing || fill(p) == RING) pthread_cond_wait(&p->c, &p->m);
        unsigned room = RING - fill(p), n = (unsigned)(len - off);
        if (n > room) n = room;
        unsigned w = p->wr & (RING - 1), first = RING - w;
        if (first > n) first = n;
        memcpy(p->ring + w, data + off, first);
        if (n > first) memcpy(p->ring, data + off + first, n - first);
        p->wr += n;
        off += (int)n;
        if (!p->started) {
            if (fill(p) >= PRIME) { feed(p); AudioQueueStart(p->q, 0); p->started = 1; }
        } else if (p->priming) {
            if (fill(p) >= REPRIME) { p->priming = 0; p->fresh = 0; if (!p->paused) feed(p); }
        } else if (!p->paused) {
            int idle = 0;
            for (int i = 0; i < NBUF; i++) idle += p->state[i] == B_IDLE;
            if (idle == NBUF && !p->fresh) p->underruns++;   /* queue ran dry mid-stream */
            p->fresh = 0;
            feed(p);    /* restart buffers that ran dry or were reclaimed by a flush */
        }
    }
    pthread_mutex_unlock(&p->m);
}

extern OSStatus AudioQueuePause(AudioQueueRef);
/* pause keeps buffered audio; resume continues where it stopped */
void aq_pause(void* h) {
    Player* p=(Player*)h;
    if (!p) return;
    pthread_mutex_lock(&p->m);
    p->paused = 1;
    if (p->started) AudioQueuePause(p->q);
    pthread_mutex_unlock(&p->m);
}
void aq_resume(void* h) {
    Player* p=(Player*)h;
    if (!p) return;
    pthread_mutex_lock(&p->m);
    p->paused = 0;
    p->fresh = 1;
    if (p->started) { feed(p); AudioQueueStart(p->q, 0); }
    pthread_mutex_unlock(&p->m);
}

extern OSStatus AudioQueueReset(AudioQueueRef);
/* Drop buffered audio immediately (track change / seek). iOS 9 does not reliably call the output
   callback for buffers discarded by Reset, so reclaim every queued buffer ourselves. The queue
   keeps running; the next aq_write primes it again. */
void aq_flush(void* h) {
    Player* p = (Player*)h;
    if (!p) return;
    pthread_mutex_lock(&p->m);
    p->flushing = 1;
    p->rd = p->wr;
    pthread_mutex_unlock(&p->m);
    if (p->started) AudioQueueReset(p->q);
    pthread_mutex_lock(&p->m);
    for (int i = 0; i < NBUF; i++) p->state[i] = B_IDLE;
    p->rd = p->wr;
    p->priming = 1;
    p->flushing = 0;
    pthread_cond_broadcast(&p->c);
    pthread_mutex_unlock(&p->m);
}

unsigned aq_underruns(void) { return P.underruns; }
unsigned aq_buffered_ms(void) { return fill(&P) / (44100 * 4 / 1000); }

/* Raise the calling (decoder) thread to round-robin realtime so background work (warmup,
   metadata, HTTP) never starves it. Darwin: SCHED_RR = 2, struct sched_param = {int; char[4]}. */
struct sched_param_ { int sched_priority; char opaque[4]; };
extern pthread_t pthread_self(void);
extern int pthread_setschedparam(pthread_t, int, const struct sched_param_*);
void aq_boost_thread(void) {
    struct sched_param_ sp = { 45, {0} };
    int r = pthread_setschedparam(pthread_self(), 2, &sp);
    printf("[aq] decoder thread SCHED_RR 45 -> %d\n", r);
}
