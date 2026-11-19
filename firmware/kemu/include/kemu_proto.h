/**
 * @file kemu_proto.h
 * @brief The wire between kemu and the Mac: 'K' 'D', type u8, len u32 LE, payload.
 *
 * crates/kode-sim/src/proto.rs is the other half of this file; the two keep the same
 * numbers. Pure C, no ESP-IDF, so it builds and is tested on the host.
 */
#ifndef KEMU_PROTO_H_
#define KEMU_PROTO_H_

#include <stddef.h>
#include <stdint.h>

#define KEMU_PROTO_VERSION 1U
#define KEMU_FRAME_HEADER  7U

/* firmware -> Mac */
#define KEMU_T_HELLO      0x01U  /* proto u16, w u16, h u16                  */
#define KEMU_T_FB_RECT    0x10U  /* x,y,w,h u16, then w*h RGB565 LE         */
#define KEMU_T_PANEL      0x11U  /* on u8, brightness u8 (0-100)             */
#define KEMU_T_FB_PACKED  0x12U  /* x,y,w,h u16, then w*h RGB565 packed     */
                                 /* with kemu_pack16(); the Mac sees FB_RECT */
#define KEMU_T_USB_IN     0x60U  /* ep_addr u8, then what the device sent: one  */
                                 /* IN transfer, whole (an ack is zero bytes)   */
#define KEMU_T_USB_DEV    0x61U  /* kind u8, a u8, b u16: what the device did  */
                                 /* to the bus (KEMU_USB_DEV_* in kemu_usbdcd.h) */
#define KEMU_T_USB_ACK    0x62U  /* ep u8, bytes u32: OUT bytes the app took, so */
                                 /* the Mac may send that many more             */
/* Mac -> firmware */
#define KEMU_T_CONSOLE_IN 0x84U  /* raw console input (the USJ's RX)        */
#define KEMU_T_TIME       0x88U  /* unix_us u64: the Mac's clock, for the RTC */
#define KEMU_T_USB_BUS    0x8AU  /* kind u8: 0 cable out, 1 cable in, 2 suspend, 3 resume, 4 reset */
#define KEMU_T_USB_SETUP  0x8BU  /* the 8 bytes of a SETUP packet           */
#define KEMU_T_USB_OUT    0x8CU  /* ep u8 (0-7), then one OUT chunk, up to 4096 bytes */

/*
 * Not the firmware's: frames between the page and the Mac, about the
 * board's chips and its sound, which kode-sim answers itself on esp-emu's
 * I2C, GPIO and I2S.
 * kemu never sends or takes them; they are here so the numbers stay
 * taken, and the page's frames share one numbering with kemu's.
 */
/* chips -> page (through the Mac) */
#define KEMU_T_LED        0x20U  /* r,g,b u8                                 */
#define KEMU_T_HAPTIC     0x21U  /* strength u8 (0-100), ms u16              */
#define KEMU_T_POWER_OFF  0x40U  /* -: the charger switched the board off   */
#define KEMU_T_PCM_OUT    0x30U  /* rate u32, ch u8, then s16le: the speaker */
#define KEMU_T_MIC_START  0x31U  /* rate u32, ch u8: the Mac's mic is on     */
#define KEMU_T_MIC_STOP   0x32U  /* -: it is off                             */
/* page -> chips (through the Mac) */
#define KEMU_T_TOUCH      0x80U  /* pressed u8, x u16, y u16                 */
#define KEMU_T_BUTTONS    0x81U  /* mask u8: KEMU_BTN_*                      */
#define KEMU_T_PCM_IN     0x82U  /* s16le at the last MIC_START rate         */
#define KEMU_T_BATTERY    0x83U  /* percent u8, usb u8                       */

#define KEMU_BTN_TOP   (1U << 0)
#define KEMU_BTN_BOT   (1U << 1)
#define KEMU_BTN_LEFT  (1U << 2)
#define KEMU_BTN_RIGHT (1U << 3)
#define KEMU_BTN_UP    (1U << 4)
#define KEMU_BTN_DOWN  (1U << 5)

typedef void (*kemu_frame_cb_t)(uint8_t type, const uint8_t *p, uint32_t len,
                                void *arg);

/** A decoder over a caller-owned buffer; a length longer than it is corruption. */
typedef struct kemu_dec {
    uint8_t *buf;
    size_t cap;
    size_t have;      /* bytes in buf: header, then payload            */
    uint32_t len;     /* payload length, once the header is complete   */
} kemu_dec_t;

/** Write the 7-byte header for a payload of @p len into @p out; returns 7. */
size_t kemu_frame_header(uint8_t out[KEMU_FRAME_HEADER], uint8_t type,
                         uint32_t len);

void kemu_dec_init(kemu_dec_t *d, uint8_t *buf, size_t cap);

/** The most kemu_pack16() writes for @p n pixels: all literals. */
#define KEMU_PACK16_MAX(n) ((size_t)(n) * 2U + ((size_t)(n) + 127U) / 128U)

/**
 * PackBits over RGB565 pixels. A control byte c < 128 is followed by c + 1
 * literal pixels; c >= 128 by one pixel repeated c - 126 times (2 to 129).
 * Pixels go out little-endian. Returns the bytes written to @p out, which
 * holds at least KEMU_PACK16_MAX(n).
 */
size_t kemu_pack16(const uint16_t *px, size_t n, uint8_t *out);

/** Feed bytes; calls @p on_frame per complete frame; returns how many. */
int kemu_dec_feed(kemu_dec_t *d, const uint8_t *in, size_t n,
                  kemu_frame_cb_t on_frame, void *arg);

#endif /* KEMU_PROTO_H_ */
