/**
 * @file kemu_usbdcd.h
 * @brief A TinyUSB device controller with a computer in place of the cable.
 *
 * TinyUSB talks to the silicon through a dozen dcd_* calls and listens to it
 * through dcd_event_*. This is a controller made of those: the "bus" is the
 * Mac, which sends SETUP packets and OUT data, takes IN data, and plugs and
 * unplugs the cable (USB_BUS, USB_SETUP, USB_OUT, USB_IN, USB_DEV, USB_ACK in
 * kemu_proto.h). The stack, its classes and the app above it run unchanged.
 *
 * Pure C with no ESP-IDF and no TinyUSB, so it builds and is tested on the
 * host (firmware/tests/test_usbdcd.c). kemu_usb.c is the glue: the dcd_*
 * wrappers, a lock, and the delivery of the events this model queues.
 *
 * What a real bus does that this one does not: no packets of 64 bytes (an IN
 * transfer goes out whole, an OUT chunk comes in whole), no NAK timing, no
 * bandwidth. A transfer is "a thing the app armed"; it completes when the
 * Mac has taken it, or sent what it waits for.
 */
#ifndef KEMU_USBDCD_H_
#define KEMU_USBDCD_H_

#include <stdbool.h>
#include <stdint.h>

#define KEMU_USB_EPS        8U      /* endpoint numbers 0..7, each with an IN and an OUT side */
#define KEMU_USB_EV_MAX     16U
#define KEMU_USB_CREDIT     8192U   /* bytes the Mac may have outstanding to one OUT endpoint */

/* USB_BUS kinds (Mac -> firmware) */
#define KEMU_USB_BUS_CABLE_OUT 0U
#define KEMU_USB_BUS_CABLE_IN  1U
#define KEMU_USB_BUS_SUSPEND   2U
#define KEMU_USB_BUS_RESUME    3U
#define KEMU_USB_BUS_RESET     4U

/* USB_DEV kinds (firmware -> Mac): what the device did to the bus */
#define KEMU_USB_DEV_PULLUP     1U  /* a: 1 connected, 0 disconnected */
#define KEMU_USB_DEV_STALL      2U  /* a: endpoint address */
#define KEMU_USB_DEV_CLEAR      3U  /* a: endpoint address */
#define KEMU_USB_DEV_EP_OPEN    4U  /* a: endpoint address, b: max packet | type << 12 */
#define KEMU_USB_DEV_ADDRESS    5U  /* a: the address the host set */
#define KEMU_USB_DEV_EP_CLOSE   6U  /* a: endpoint address, or 0xFF for all */

/* What the stack is told, in the order it is told it. */
typedef enum {
    KEMU_USB_EV_BUS_RESET = 1,
    KEMU_USB_EV_UNPLUGGED,
    KEMU_USB_EV_SUSPEND,
    KEMU_USB_EV_RESUME,
    KEMU_USB_EV_SETUP,
    KEMU_USB_EV_XFER,
} kemu_usb_ev_kind_t;

typedef struct {
    kemu_usb_ev_kind_t kind;
    uint8_t setup[8];       /* SETUP */
    uint8_t ep_addr;        /* XFER */
    uint32_t len;           /* XFER: bytes moved */
    bool ok;                /* XFER: false when it was stalled or cut off */
} kemu_usb_ev_t;

/** What the model sends to the Mac. */
typedef struct {
    void (*in_data)(void *ctx, uint8_t ep_addr, const uint8_t *p, uint32_t len);    /* USB_IN */
    void (*dev)(void *ctx, uint8_t kind, uint8_t a, uint16_t b);                    /* USB_DEV */
    void (*ack)(void *ctx, uint8_t ep, uint32_t bytes);                             /* USB_ACK */
} kemu_usb_out_t;

typedef struct kemu_usb_chunk kemu_usb_chunk_t;

typedef struct {
    bool open;
    bool stalled;
    uint16_t mps;
    uint8_t type;                   /* 0 control, 1 iso, 2 bulk, 3 interrupt */
    /* an armed transfer */
    bool armed;
    uint8_t *buf;
    uint32_t total, done;
    /* OUT: what the Mac sent and the app has not asked for yet */
    kemu_usb_chunk_t *q_head, *q_tail;
    uint32_t q_bytes;
} kemu_usb_ep_t;

typedef struct {
    kemu_usb_out_t out;
    void *ctx;
    bool inited;                    /* dcd_init */
    bool pullup;                    /* dcd_connect */
    bool cable;                     /* the Mac plugged it in */
    bool on_bus;                    /* pullup and cable: the host has been told, and reset the bus */
    kemu_usb_ep_t in[KEMU_USB_EPS], outp[KEMU_USB_EPS];
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint32_t nev;
} kemu_usb_t;

void kemu_usb_init(kemu_usb_t *m, kemu_usb_out_t out, void *ctx);

/* ---- the stack's side: dcd_* ---- */
bool kemu_usb_dcd_init(kemu_usb_t *m);
void kemu_usb_dcd_deinit(kemu_usb_t *m);
void kemu_usb_dcd_connect(kemu_usb_t *m);
void kemu_usb_dcd_disconnect(kemu_usb_t *m);
void kemu_usb_dcd_set_address(kemu_usb_t *m, uint8_t addr);
bool kemu_usb_dcd_edpt_open(kemu_usb_t *m, uint8_t ep_addr, uint16_t mps, uint8_t type);
void kemu_usb_dcd_edpt_close(kemu_usb_t *m, uint8_t ep_addr);
void kemu_usb_dcd_edpt_close_all(kemu_usb_t *m);
bool kemu_usb_dcd_edpt_xfer(kemu_usb_t *m, uint8_t ep_addr, uint8_t *buf, uint16_t total);
void kemu_usb_dcd_edpt_stall(kemu_usb_t *m, uint8_t ep_addr);
void kemu_usb_dcd_edpt_clear_stall(kemu_usb_t *m, uint8_t ep_addr);

/* ---- the Mac's side ---- */
void kemu_usb_bus(kemu_usb_t *m, uint8_t kind);
void kemu_usb_setup(kemu_usb_t *m, const uint8_t setup[8]);
/** OUT data for endpoint @p ep (0-7). Returns false when it was refused (no such endpoint). */
bool kemu_usb_out_data(kemu_usb_t *m, uint8_t ep, const uint8_t *p, uint32_t len);

/** Take the queued events (up to @p cap) off the model, in order. */
uint32_t kemu_usb_take_events(kemu_usb_t *m, kemu_usb_ev_t *dst, uint32_t cap);

#endif /* KEMU_USBDCD_H_ */
