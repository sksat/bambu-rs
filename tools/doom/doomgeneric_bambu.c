// The platform layer that turns doomgeneric into a frame source for
// `bambu serve --emulate-doom`.
//
// doomgeneric (github.com/ozkl/doomgeneric) is DOOM with its platform layer
// removed: it renders into DG_ScreenBuffer and asks the host for a clock, a
// sleep and the keyboard. This is that host, with a pipe on each side —
//
//   stdout : the printer's own camera framing, which is a 16-byte header
//            (little-endian u32 length, then 0, 1, 0) followed by a baseline
//            JPEG. That is exactly what `bambu serve` serves on TCP 6000, so
//            the frames need no transcoding on the way to a client's liveview.
//            With -raw the headers are left off and the output is a plain
//            concatenation of JPEGs, which `ffplay -f mjpeg -` will play — the
//            fastest way to tell "the engine is broken" from "the relay is".
//   stdin  : key events, two bytes each: [pressed (0|1), doom key code].
//            DG_GetKey's own shape, flattened onto a pipe. EOF means the host
//            has gone away, and so do we.
//
// Everything DOOM itself prints goes to stderr, because a single stray printf
// on stdout would be read as part of a picture.
//
// Not part of the bambu-rs build: this is built on demand by ./build.sh, which
// fetches doomgeneric and stb_image_write.h. See README.md.

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include "doomgeneric.h"
#include "doomkeys.h"
#include "doomstat.h"  // gamestate, players[], consoleplayer — for the status record
#include "m_argv.h"

#define STB_IMAGE_WRITE_IMPLEMENTATION
// Frames go out over a pipe, never to a file.
#define STBI_WRITE_NO_STDIO
#include "stb_image_write.h"

// What a client expects to be shown. 1280x720 is the size Bambu Studio is known
// to display; the render is letterboxed into it rather than stretched, because
// a stretched DOOM is a DOOM nobody wants to look at.
#define OUT_W 1280
#define OUT_H 720

// The frame header the printer's camera port puts in front of every picture:
// four little-endian words — length, 0, 1, 0.
#define FRAME_HEADER 16
// The smallest frame the relay's own client will accept. A picture below this
// is treated as an error page, so sending one is worse than sending nothing.
#define MIN_FRAME 1000

// A status record shares the pipe with the pictures, and says how the player is
// doing so the relay can report health as the nozzle temperature.
//
// Its length word is ZERO, which is the whole trick: no frame may be shorter
// than MIN_FRAME, so a reader that knows only about frames refuses this outright
// instead of handing four bytes of binary to a JPEG decoder. The magic sits in
// the word a frame leaves at zero, and is readable in a hexdump, which is the
// only debugger this pipe has.
//
//   0..4   0
//   4..8   "DOOM"
//   8..12  payload length
//   12..16 0
//   payload: health int16le, armour int16le  (negative = no player)
//
// The Rust side of this is `status_header`/`parse_vitals` in src/core/doom.rs.
#define STATUS_MAGIC "DOOM"
#define STATUS_PAYLOAD 4

// Below 90, stb writes 4:2:0 chroma subsampling, which is the baseline JPEG
// profile a printer's camera produces and the one clients are known to decode.
#define DEFAULT_QUALITY 70

static int frame_fd = -1;   // the real stdout, saved before DOOM gets hold of it
static int raw_output = 0;  // -raw: no frame headers, for `ffplay -f mjpeg -`
static int quality = DEFAULT_QUALITY;
static uint32_t min_frame_interval_ms = 0;  // -maxfps: 0 means "as fast as DOOM runs"
static uint32_t last_frame_ms = 0;
static uint64_t frames_sent = 0;

static unsigned char rgb[OUT_W * OUT_H * 3];

// ---- the outgoing frame ---------------------------------------------------

struct jpeg_buf {
    unsigned char* data;
    int len;
    int cap;
};

static void jpeg_write(void* context, void* data, int size) {
    struct jpeg_buf* buf = (struct jpeg_buf*)context;
    if (buf->len + size > buf->cap) {
        int cap = buf->cap ? buf->cap * 2 : 256 * 1024;
        while (cap < buf->len + size) cap *= 2;
        unsigned char* grown = (unsigned char*)realloc(buf->data, cap);
        if (!grown) {
            fprintf(stderr, "doom-engine: out of memory encoding a frame\n");
            exit(1);
        }
        buf->data = grown;
        buf->cap = cap;
    }
    memcpy(buf->data + buf->len, data, size);
    buf->len += size;
}

// A pipe write can be partial; a frame written partially is a frame the reader
// will never resynchronise from.
static int write_all(const unsigned char* p, size_t n) {
    while (n > 0) {
        ssize_t w = write(frame_fd, p, n);
        if (w < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        p += w;
        n -= (size_t)w;
    }
    return 0;
}

// Nearest-neighbour, aspect-preserving, into a black field. Nearest rather than
// anything smoother on purpose: DOOM is 320x200 blown up, and blurring it just
// makes the pixels look like a mistake.
static void scale_into_rgb(void) {
    const int src_w = DOOMGENERIC_RESX;
    const int src_h = DOOMGENERIC_RESY;
    // The largest whole rectangle of the target that keeps the source's shape.
    int dst_w = OUT_W;
    int dst_h = (int)((int64_t)src_h * OUT_W / src_w);
    if (dst_h > OUT_H) {
        dst_h = OUT_H;
        dst_w = (int)((int64_t)src_w * OUT_H / src_h);
    }
    const int x0 = (OUT_W - dst_w) / 2;
    const int y0 = (OUT_H - dst_h) / 2;

    memset(rgb, 0, sizeof(rgb));
    for (int y = 0; y < dst_h; y++) {
        const int sy = (int)((int64_t)y * src_h / dst_h);
        const pixel_t* src_row = DG_ScreenBuffer + (size_t)sy * src_w;
        unsigned char* dst_row = rgb + ((size_t)(y0 + y) * OUT_W + x0) * 3;
        for (int x = 0; x < dst_w; x++) {
            const int sx = (int)((int64_t)x * src_w / dst_w);
            // doomgeneric's default framebuffer is XRGB8888.
            const uint32_t p = (uint32_t)src_row[sx];
            dst_row[x * 3 + 0] = (unsigned char)((p >> 16) & 0xFF);
            dst_row[x * 3 + 1] = (unsigned char)((p >> 8) & 0xFF);
            dst_row[x * 3 + 2] = (unsigned char)(p & 0xFF);
        }
    }
}

// Tell the relay how the player is doing, if it has changed.
//
// Sent before the frame-rate gate, so throttling the picture does not also
// freeze the health: the record is 20 bytes and the reading is what a client
// sees on its temperature readout.
static void write_status(void) {
    static int16_t last_health = -2, last_armour = -2;  // -1 is a real answer
    int16_t health = -1, armour = -1;
    // Only inside a level is there a player to ask about. At the title screen
    // and between maps the fields hold whatever they last held, and reporting
    // that would be a health bar that lies while the game is not running.
    if (gamestate == GS_LEVEL && playeringame[consoleplayer]) {
        health = (int16_t)players[consoleplayer].health;
        armour = (int16_t)players[consoleplayer].armorpoints;
    }
    if (health == last_health && armour == last_armour) {
        return;
    }
    last_health = health;
    last_armour = armour;

    unsigned char record[FRAME_HEADER + STATUS_PAYLOAD];
    memset(record, 0, sizeof(record));
    memcpy(record + 4, STATUS_MAGIC, 4);
    record[8] = STATUS_PAYLOAD;
    record[16] = (unsigned char)(health & 0xFF);
    record[17] = (unsigned char)((health >> 8) & 0xFF);
    record[18] = (unsigned char)(armour & 0xFF);
    record[19] = (unsigned char)((armour >> 8) & 0xFF);
    if (write_all(record, sizeof(record)) != 0) {
        fprintf(stderr, "doom-engine: the host stopped reading; stopping\n");
        exit(0);
    }
}

void DG_DrawFrame(void) {
    // Before the gate below: `-raw` is for ffplay, which would choke on a
    // record it has no idea about, and everything else wants the health as soon
    // as it moves.
    if (!raw_output) {
        write_status();
    }

    const uint32_t now = DG_GetTicksMs();
    if (min_frame_interval_ms && frames_sent &&
        (uint32_t)(now - last_frame_ms) < min_frame_interval_ms) {
        return;  // -maxfps: the game keeps running, the viewer sees fewer frames
    }
    last_frame_ms = now;

    scale_into_rgb();

    static struct jpeg_buf out;
    out.len = 0;
    if (!stbi_write_jpg_to_func(jpeg_write, &out, OUT_W, OUT_H, 3, rgb, quality)) {
        fprintf(stderr, "doom-engine: JPEG encoding failed\n");
        return;
    }
    // A frame this small is refused at the far end as an error page rather than
    // a photograph, so sending it would be a decode failure instead of a
    // skipped frame. Only a near-uniform screen can get here.
    if (out.len < MIN_FRAME) {
        fprintf(stderr, "doom-engine: skipped a %d-byte frame (below the %d-byte floor)\n",
                out.len, MIN_FRAME);
        return;
    }

    if (!raw_output) {
        unsigned char header[FRAME_HEADER] = {0};
        const uint32_t len = (uint32_t)out.len;
        header[0] = (unsigned char)(len & 0xFF);
        header[1] = (unsigned char)((len >> 8) & 0xFF);
        header[2] = (unsigned char)((len >> 16) & 0xFF);
        header[3] = (unsigned char)((len >> 24) & 0xFF);
        header[8] = 1;  // the word every published reader of this stream expects
        if (write_all(header, sizeof(header)) != 0) {
            fprintf(stderr, "doom-engine: the host stopped reading; stopping\n");
            exit(0);
        }
    }
    if (write_all(out.data, (size_t)out.len) != 0) {
        fprintf(stderr, "doom-engine: the host stopped reading; stopping\n");
        exit(0);
    }
    if (frames_sent == 0) {
        // The one line that separates "the engine never started" from "the
        // relay never showed it", which look identical from a client.
        fprintf(stderr, "doom-engine: first frame out, %d bytes, %dx%d\n", out.len, OUT_W, OUT_H);
    }
    frames_sent++;
}

// ---- the incoming keys ----------------------------------------------------

#define KEY_QUEUE 256
static unsigned short key_queue[KEY_QUEUE];
static unsigned int key_write, key_read;
static unsigned char pending[2];
static int pending_len;

static void queue_key(int pressed, unsigned char key) {
    key_queue[key_write] = (unsigned short)((pressed ? 1 : 0) << 8 | key);
    key_write = (key_write + 1) % KEY_QUEUE;
    // Overrunning drops the oldest event, which is the right one to lose: the
    // newest is what the operator just pressed.
    if (key_write == key_read) key_read = (key_read + 1) % KEY_QUEUE;
}

static void pump_stdin(void) {
    unsigned char buf[256];
    for (;;) {
        ssize_t n = read(STDIN_FILENO, buf, sizeof(buf));
        if (n < 0) {
            if (errno == EINTR) continue;
            return;  // EAGAIN: nothing waiting, which is the usual answer
        }
        if (n == 0) {
            // The host closed the pipe. Nothing will ever press a key again,
            // and a DOOM left running would hold the port after `serve` exits.
            fprintf(stderr, "doom-engine: input closed; exiting\n");
            exit(0);
        }
        for (ssize_t i = 0; i < n; i++) {
            pending[pending_len++] = buf[i];
            if (pending_len == 2) {
                queue_key(pending[0], pending[1]);
                pending_len = 0;
            }
        }
    }
}

int DG_GetKey(int* pressed, unsigned char* key) {
    pump_stdin();
    if (key_read == key_write) return 0;
    const unsigned short event = key_queue[key_read];
    key_read = (key_read + 1) % KEY_QUEUE;
    *pressed = (event >> 8) & 0xFF;
    *key = event & 0xFF;
    return 1;
}

// ---- the rest of the platform --------------------------------------------

void DG_Init(void) {
    // Non-blocking, because DG_GetKey is called from inside the game loop and
    // must never wait for an operator who is not there.
    const int flags = fcntl(STDIN_FILENO, F_GETFL, 0);
    if (flags < 0 || fcntl(STDIN_FILENO, F_SETFL, flags | O_NONBLOCK) < 0) {
        fprintf(stderr, "doom-engine: cannot make stdin non-blocking: %s\n", strerror(errno));
        exit(1);
    }
}

uint32_t DG_GetTicksMs(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint32_t)(ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
}

void DG_SleepMs(uint32_t ms) {
    struct timespec ts = {.tv_sec = ms / 1000, .tv_nsec = (long)(ms % 1000) * 1000000};
    nanosleep(&ts, NULL);
}

void DG_SetWindowTitle(const char* title) { (void)title; }

int main(int argc, char** argv) {
    // Take the real stdout away before DOOM can print to it: i_video and the
    // WAD loader are chatty, and one line of that inside the stream is a
    // corrupt frame the reader cannot resynchronise from.
    frame_fd = dup(STDOUT_FILENO);
    if (frame_fd < 0 || dup2(STDERR_FILENO, STDOUT_FILENO) < 0) {
        fprintf(stderr, "doom-engine: cannot separate the frame stream from the log\n");
        return 1;
    }

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "-raw") == 0) {
            raw_output = 1;
        } else if (strcmp(argv[i], "-quality") == 0 && i + 1 < argc) {
            quality = atoi(argv[++i]);
            if (quality < 1 || quality > 89) {
                // 90 and above switches stb to 4:4:4, which is not the baseline
                // 4:2:0 profile a printer's camera emits.
                fprintf(stderr, "doom-engine: -quality must be 1..89 (4:2:0 baseline)\n");
                return 2;
            }
        } else if (strcmp(argv[i], "-maxfps") == 0 && i + 1 < argc) {
            const int fps = atoi(argv[++i]);
            if (fps < 1) {
                fprintf(stderr, "doom-engine: -maxfps must be at least 1\n");
                return 2;
            }
            min_frame_interval_ms = (uint32_t)(1000 / fps);
        } else if (strcmp(argv[i], "-workdir") == 0 && i + 1 < argc) {
            // DOOM writes `.default.cfg` and `.savegame/` into its working
            // directory, and this one inherits `bambu serve`'s — so without
            // this the demo drops those into whatever repository checkout the
            // relay happened to be started from. Loud on failure: a game that
            // quietly ran somewhere else would find no WAD and say so in a
            // much less useful way.
            const char* dir = argv[++i];
            if (mkdir(dir, 0777) != 0 && errno != EEXIST) {
                fprintf(stderr, "doom-engine: cannot make %s: %s\n", dir, strerror(errno));
                return 2;
            }
            if (chdir(dir) != 0) {
                fprintf(stderr, "doom-engine: cannot enter %s: %s\n", dir, strerror(errno));
                return 2;
            }
            // Said out loud because it moves the ground under every relative
            // path that follows, `-iwad` included.
            fprintf(stderr, "doom-engine: saves and config go in %s\n", dir);
        }
        // Everything else is DOOM's: -iwad, -warp, -skill, …
    }

    doomgeneric_Create(argc, argv);
    for (;;) {
        doomgeneric_Tick();
    }
    return 0;
}
