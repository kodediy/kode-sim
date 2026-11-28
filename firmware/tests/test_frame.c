#include <string.h>
#include "check.h"
#include "kemu_proto.h"

static uint8_t got_type[8];
static uint8_t got_payload[8][16];
static uint32_t got_len[8];
static int got_n;

static void on_frame(uint8_t type, const uint8_t *p, uint32_t len, void *arg)
{
    (void)arg;
    if (got_n < 8) {
        got_type[got_n] = type;
        got_len[got_n] = len;
        memcpy(got_payload[got_n], p, len < 16 ? len : 16);
    }
    got_n++;
}

static size_t frame(uint8_t *out, uint8_t type, const uint8_t *p, uint32_t len)
{
    size_t h = kemu_frame_header(out, type, len);
    memcpy(out + h, p, len);
    return h + len;
}

static uint8_t dec_buf[64];
static kemu_dec_t dec;

static void reset(void)
{
    got_n = 0;
    kemu_dec_init(&dec, dec_buf, sizeof dec_buf);
}

static void roundtrip(void)
{
    uint8_t f[32];
    const uint8_t red[3] = {255, 0, 0};
    reset();
    size_t n = frame(f, KEMU_T_LED, red, 3);
    CHECK(kemu_dec_feed(&dec, f, n, on_frame, NULL) == 1);
    CHECK(got_n == 1 && got_type[0] == KEMU_T_LED && got_len[0] == 3);
    CHECK(memcmp(got_payload[0], red, 3) == 0);
}

static void split_reads(void)
{
    uint8_t f[32];
    const uint8_t t[5] = {1, 10, 0, 20, 0};
    reset();
    size_t n = frame(f, KEMU_T_TOUCH, t, 5);
    int frames = 0;
    for (size_t i = 0; i < n; i++) {
        frames += kemu_dec_feed(&dec, &f[i], 1, on_frame, NULL);
    }
    CHECK(frames == 1 && got_n == 1 && got_type[0] == KEMU_T_TOUCH);
}

/* A feed that ends inside a payload, and one that carries its tail, a whole
 * frame and the next one's first bytes: what UART reads cut out of a stream. */
static void payload_split_across_feeds(void)
{
    uint8_t f[64];
    const uint8_t a[12] = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12};
    const uint8_t b[2] = {0xAA, 0xBB};
    const uint8_t c[5] = {'K', 'D', 'K', 'D', 0};
    size_t n = 0;
    reset();
    n += frame(f + n, KEMU_T_USB_OUT, a, sizeof a);
    n += frame(f + n, KEMU_T_LED, b, sizeof b);
    n += frame(f + n, KEMU_T_TOUCH, c, sizeof c);
    CHECK(kemu_dec_feed(&dec, f, 12, on_frame, NULL) == 0);          /* 5 bytes into a */
    CHECK(kemu_dec_feed(&dec, f + 12, n - 12 - 3, on_frame, NULL) == 2);
    CHECK(kemu_dec_feed(&dec, f + n - 3, 3, on_frame, NULL) == 1);   /* c's tail */
    CHECK(got_n == 3);
    CHECK(got_type[0] == KEMU_T_USB_OUT && got_len[0] == 12 && memcmp(got_payload[0], a, 12) == 0);
    CHECK(got_type[1] == KEMU_T_LED && got_len[1] == 2 && memcmp(got_payload[1], b, 2) == 0);
    CHECK(got_type[2] == KEMU_T_TOUCH && got_len[2] == 5 && memcmp(got_payload[2], c, 5) == 0);
}

/* A payload that fills the buffer to the last byte, twice in one feed. */
static void payload_fills_the_buffer(void)
{
    static uint8_t f[2 * sizeof dec_buf];
    uint8_t p[sizeof dec_buf - KEMU_FRAME_HEADER];
    size_t n = 0;
    reset();
    for (size_t i = 0; i < sizeof p; i++) {
        p[i] = (uint8_t)(i * 7U);
    }
    n += frame(f + n, KEMU_T_USB_OUT, p, sizeof p);
    n += frame(f + n, KEMU_T_USB_OUT, p, sizeof p);
    CHECK(kemu_dec_feed(&dec, f, n, on_frame, NULL) == 2);
    CHECK(got_n == 2 && got_len[1] == sizeof p && memcmp(got_payload[1], p, 16) == 0);
    CHECK(memcmp(dec_buf + KEMU_FRAME_HEADER, p, sizeof p) == 0);
}

static void garbage_resync(void)
{
    uint8_t f[64];
    const uint8_t h[6] = {1, 0, 0x9a, 1, 0xf6, 1};
    size_t n = 0;
    reset();
    memcpy(f, "\x00\xffKX", 4); n = 4;
    n += frame(f + n, KEMU_T_HELLO, h, 6);
    memcpy(f + n, "junk", 4); n += 4;
    n += frame(f + n, KEMU_T_HELLO, h, 6);
    CHECK(kemu_dec_feed(&dec, f, n, on_frame, NULL) == 2);
    CHECK(got_n == 2 && got_len[1] == 6);
}

static void oversize_dropped(void)
{
    uint8_t f[64];
    const uint8_t led[3] = {1, 2, 3};
    size_t n;
    reset();
    n = kemu_frame_header(f, KEMU_T_FB_RECT, 1000); /* bigger than the 64-byte buffer */
    memset(f + n, 0, 16); n += 16;
    n += frame(f + n, KEMU_T_LED, led, 3);
    CHECK(kemu_dec_feed(&dec, f, n, on_frame, NULL) == 1);
    CHECK(got_n == 1 && got_type[0] == KEMU_T_LED);
}

/* A length that, added to the header on a 32-bit size_t, wraps to a few
 * bytes: it is corruption like any other too-long length, never a frame. */
static void wrapping_length_is_corruption(void)
{
    uint8_t f[32];
    const uint8_t led[3] = {4, 5, 6};
    size_t n;
    reset();
    memset(dec_buf, 0xEE, sizeof dec_buf);
    n = kemu_frame_header(f, KEMU_T_TOUCH, 0xFFFFFFF9U);
    n += frame(f + n, KEMU_T_LED, led, 3);
    CHECK(kemu_dec_feed(&dec, f, n, on_frame, NULL) == 1);
    CHECK(got_n == 1 && got_type[0] == KEMU_T_LED);
}

/* The vectors kode-emu's own tests unpack too (crates/kode-emu/src/proto.rs). */
static void pack16_runs_and_literals(void)
{
    const uint16_t px[] = {5, 5, 5, 1, 2, 3, 3};
    const uint8_t want[] = {0x81, 5, 0,  0x01, 1, 0, 2, 0,  0x80, 3, 0};
    uint8_t out[KEMU_PACK16_MAX(7)];
    CHECK(kemu_pack16(px, 7, out) == sizeof want);
    CHECK(memcmp(out, want, sizeof want) == 0);
}

static void pack16_long_run_splits_at_129(void)
{
    uint16_t px[300];
    uint8_t out[KEMU_PACK16_MAX(300)];
    const uint8_t want[] = {0xFF, 0x34, 0x12,  0xFF, 0x34, 0x12,  0xA8, 0x34, 0x12};
    for (int i = 0; i < 300; i++) {
        px[i] = 0x1234;
    }
    CHECK(kemu_pack16(px, 300, out) == sizeof want);
    CHECK(memcmp(out, want, sizeof want) == 0);
}

static void pack16_long_literal_splits_at_128(void)
{
    uint16_t px[130];
    uint8_t out[KEMU_PACK16_MAX(130)];
    for (int i = 0; i < 130; i++) {
        px[i] = (uint16_t)i;
    }
    CHECK(kemu_pack16(px, 130, out) == 1 + 256 + 1 + 4);
    CHECK(KEMU_PACK16_MAX(130) >= 1 + 256 + 1 + 4);
    CHECK(out[0] == 0x7F);
    CHECK(out[257] == 0x01);
    CHECK(out[258] == 128 && out[260] == 129);
}

static void pack16_single_pixel(void)
{
    const uint16_t px[] = {7};
    uint8_t out[KEMU_PACK16_MAX(1)];
    CHECK(kemu_pack16(px, 1, out) == 3);
    CHECK(out[0] == 0x00 && out[1] == 7 && out[2] == 0);
}

int main(void)
{
    RUN(roundtrip);
    RUN(split_reads);
    RUN(payload_split_across_feeds);
    RUN(payload_fills_the_buffer);
    RUN(garbage_resync);
    RUN(oversize_dropped);
    RUN(wrapping_length_is_corruption);
    RUN(pack16_runs_and_literals);
    RUN(pack16_long_run_splits_at_129);
    RUN(pack16_long_literal_splits_at_128);
    RUN(pack16_single_pixel);
    CHECK_DONE();
}
