/**
 * @file kemu_usb.c
 * @brief The USB-OTG port: a TinyUSB device controller with the Mac as the host.
 *
 * Every app that is a USB device (SD Reader, BadUSB, Air Mouse, Webcam) runs
 * the real TinyUSB: usb_new_phy() for the PHY, tusb_init(), then the class
 * callbacks. Under esp-emu the OTG controller is absent, so TinyUSB's device
 * controller driver (dcd_*) is what kemu replaces, with the model in
 * models/usbdcd.c: the stack, its descriptors, its classes and the app above
 * run unchanged, and the "bus" is the page. The page's computer (kode-emu's
 * usbhost.rs) enumerates the device, mounts its disk, reads its keyboard.
 *
 * usb_new_phy() and usb_del_phy() answer at once: there is no PHY to route,
 * and the app's own writes to the routing bits land in registers esp-emu
 * keeps for itself.
 *
 * tinyusb_driver_install() - esp_tinyusb, which kodeOS uses for its dock
 * port - still answers "not supported": kodeOS says so at once instead of
 * waiting five seconds for a stack that would never start.
 *
 * dcd_event_handler() is TinyUSB's; it is declared weak so an app with no
 * TinyUSB links without it (nothing there calls the wrappers either).
 */
#include <stdbool.h>
#include <stdint.h>
#include <string.h>

#include "esp_err.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"

#include "kemu_link.h"
#include "kemu_usb.h"
#include "kemu_usbdcd.h"

/* TinyUSB 0.19's dcd_event_t, byte for byte (device/dcd.h): rhport, event_id,
 * then a union that starts at offset 4. The numbers are dcd_eventid_t's. */
typedef struct {
    uint8_t rhport;
    uint8_t event_id;
    uint16_t pad;
    union {
        uint32_t speed;                                         /* bus_reset.speed */
        uint8_t setup[8];                                       /* setup_received */
        struct { uint8_t ep_addr; uint8_t result; uint16_t pad; uint32_t len; } xfer;  /* xfer_complete */
        uint32_t raw[2];
    } u;
} tu_event_t;
_Static_assert(sizeof(tu_event_t) == 12, "dcd_event_t is 12 bytes in TinyUSB 0.19");

#define TU_EVT_BUS_RESET   1U
#define TU_EVT_UNPLUGGED   2U
#define TU_EVT_SUSPEND     4U
#define TU_EVT_RESUME      5U
#define TU_EVT_SETUP       6U
#define TU_EVT_XFER        7U
#define TU_SPEED_FULL      0U

/* tusb_desc_endpoint_t, packed: bLength, bDescriptorType, bEndpointAddress, bmAttributes, wMaxPacketSize, bInterval. */
typedef struct __attribute__((packed)) {
    uint8_t bLength, bDescriptorType, bEndpointAddress, bmAttributes;
    uint16_t wMaxPacketSize;
    uint8_t bInterval;
} tu_desc_ep_t;

extern void dcd_event_handler(const tu_event_t *event, bool in_isr) __attribute__((weak));

static kemu_usb_t s_m;
static SemaphoreHandle_t s_lock;
static bool s_up;
/* dcd_int_enable()/dcd_int_disable(): the controller's interrupt. Off, nothing reaches the stack
 * (the events wait in the model): tud_deinit() turns it off before it deletes its event queue,
 * and an event sent after that would be a send on a queue that is gone. */
static volatile bool s_ints;

/* The model's sends: called with the lock held, from whichever task is in it. */
static void m_in(void *ctx, uint8_t ep_addr, const uint8_t *p, uint32_t len)
{
    (void)ctx;
    kemu_send2(KEMU_T_USB_IN, &ep_addr, 1U, p, len);
}

static void m_dev(void *ctx, uint8_t kind, uint8_t a, uint16_t b)
{
    (void)ctx;
    uint8_t const f[4] = {kind, a, (uint8_t)(b & 0xFFU), (uint8_t)(b >> 8)};
    kemu_send(KEMU_T_USB_DEV, f, sizeof f);
}

static void m_ack(void *ctx, uint8_t ep, uint32_t bytes)
{
    (void)ctx;
    uint8_t const f[5] = {ep, (uint8_t)bytes, (uint8_t)(bytes >> 8), (uint8_t)(bytes >> 16), (uint8_t)(bytes >> 24)};
    kemu_send(KEMU_T_USB_ACK, f, sizeof f);
}

static void ensure(void)
{
    if (!s_up) {
        s_lock = xSemaphoreCreateMutex();
        kemu_usb_init(&s_m, (kemu_usb_out_t){m_in, m_dev, m_ack}, NULL);
        s_up = true;
    }
}

/* What the model queued goes to the stack, outside the lock: the stack's queue may block. */
static void deliver(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint32_t const n = s_ints ? kemu_usb_take_events(&s_m, ev, KEMU_USB_EV_MAX) : 0U;
    xSemaphoreGive(s_lock);
    if (dcd_event_handler == NULL) {
        return;
    }
    for (uint32_t i = 0; i < n; i++) {
        tu_event_t t;
        memset(&t, 0, sizeof t);
        switch (ev[i].kind) {
        case KEMU_USB_EV_BUS_RESET: t.event_id = TU_EVT_BUS_RESET; t.u.speed = TU_SPEED_FULL; break;
        case KEMU_USB_EV_UNPLUGGED: t.event_id = TU_EVT_UNPLUGGED; break;
        case KEMU_USB_EV_SUSPEND:   t.event_id = TU_EVT_SUSPEND; break;
        case KEMU_USB_EV_RESUME:    t.event_id = TU_EVT_RESUME; break;
        case KEMU_USB_EV_SETUP:     t.event_id = TU_EVT_SETUP; memcpy(t.u.setup, ev[i].setup, 8); break;
        case KEMU_USB_EV_XFER:
            t.event_id = TU_EVT_XFER;
            t.u.xfer.ep_addr = ev[i].ep_addr;
            t.u.xfer.result = ev[i].ok ? 0U : 1U;
            t.u.xfer.len = ev[i].len;
            break;
        }
        dcd_event_handler(&t, false);
    }
}

#define WITH_MODEL(call) do { ensure(); xSemaphoreTake(s_lock, portMAX_DELAY); call; deliver(); } while (0)

/* ---- the Mac's frames ---- */

static void on_bus(const uint8_t *p, uint32_t len)
{
    if (len >= 1U) {
        WITH_MODEL(kemu_usb_bus(&s_m, p[0]));
    }
}

static void on_setup(const uint8_t *p, uint32_t len)
{
    if (len >= 8U) {
        WITH_MODEL(kemu_usb_setup(&s_m, p));
    }
}

static void on_out(const uint8_t *p, uint32_t len)
{
    if (len >= 1U) {
        WITH_MODEL((void)kemu_usb_out_data(&s_m, p[0], p + 1, len - 1U));
    }
}

void kemu_usb_start(void)
{
    ensure();
    kemu_on(KEMU_T_USB_BUS, on_bus);
    kemu_on(KEMU_T_USB_SETUP, on_setup);
    kemu_on(KEMU_T_USB_OUT, on_out);
}

/* ---- esp_tinyusb, and the PHY ---- */

esp_err_t __wrap_tinyusb_driver_install(const void *config)
{
    (void)config;
    return ESP_ERR_NOT_SUPPORTED;
}

static int s_phy;

esp_err_t __wrap_usb_new_phy(const void *conf, void **handle)
{
    (void)conf;
    if (handle == NULL) {
        return ESP_ERR_INVALID_ARG;
    }
    *handle = &s_phy;
    return ESP_OK;
}

esp_err_t __wrap_usb_del_phy(void *handle)
{
    (void)handle;
    /* An app that stops its USB (SD Reader after the computer ejected it) ends with tusb_deinit()
     * and this. dcd_deinit() is TinyUSB's own weak function, called inside usbd.c, so the link
     * cannot wrap it: the device leaves the bus here instead. */
    s_ints = false;
    WITH_MODEL(kemu_usb_dcd_deinit(&s_m));
    return ESP_OK;
}

/* ---- TinyUSB's device controller driver ---- */

bool __wrap_dcd_init(uint8_t rhport, const void *rh_init)
{
    (void)rhport; (void)rh_init;
    bool ok = false;
    /* dcd_init() of the real (DWC2) driver ends by raising the pull-up: the stack never
     * calls dcd_connect() itself unless tud_connect() asks again after a tud_disconnect(). */
    WITH_MODEL({ ok = kemu_usb_dcd_init(&s_m); if (ok) { kemu_usb_dcd_connect(&s_m); } });
    return ok;
}

bool __wrap_dcd_deinit(uint8_t rhport)
{
    (void)rhport;
    WITH_MODEL(kemu_usb_dcd_deinit(&s_m));
    return true;
}

void __wrap_dcd_int_handler(uint8_t rhport) { (void)rhport; }
void __wrap_dcd_int_enable(uint8_t rhport)
{
    (void)rhport;
    s_ints = true;
    WITH_MODEL((void)0);   /* what waited goes now */
}

void __wrap_dcd_int_disable(uint8_t rhport)
{
    (void)rhport;
    s_ints = false;
}
void __wrap_dcd_remote_wakeup(uint8_t rhport) { (void)rhport; }
void __wrap_dcd_sof_enable(uint8_t rhport, bool en) { (void)rhport; (void)en; }
void __wrap_dcd_edpt0_status_complete(uint8_t rhport, const void *request) { (void)rhport; (void)request; }

void __wrap_dcd_set_address(uint8_t rhport, uint8_t dev_addr)
{
    (void)rhport;
    WITH_MODEL(kemu_usb_dcd_set_address(&s_m, dev_addr));
}

void __wrap_dcd_connect(uint8_t rhport)
{
    (void)rhport;
    WITH_MODEL(kemu_usb_dcd_connect(&s_m));
}

void __wrap_dcd_disconnect(uint8_t rhport)
{
    (void)rhport;
    WITH_MODEL(kemu_usb_dcd_disconnect(&s_m));
}

bool __wrap_dcd_edpt_open(uint8_t rhport, const tu_desc_ep_t *d)
{
    (void)rhport;
    bool ok = false;
    WITH_MODEL(ok = kemu_usb_dcd_edpt_open(&s_m, d->bEndpointAddress, d->wMaxPacketSize & 0x7FFU, d->bmAttributes & 3U));
    return ok;
}

bool __wrap_dcd_edpt_iso_alloc(uint8_t rhport, uint8_t ep_addr, uint16_t largest_packet_size)
{
    (void)rhport;
    bool ok = false;
    WITH_MODEL(ok = kemu_usb_dcd_edpt_open(&s_m, ep_addr, largest_packet_size, 1U));
    return ok;
}

bool __wrap_dcd_edpt_iso_activate(uint8_t rhport, const tu_desc_ep_t *d)
{
    return __wrap_dcd_edpt_open(rhport, d);
}

void __wrap_dcd_edpt_close(uint8_t rhport, uint8_t ep_addr)
{
    (void)rhport;
    WITH_MODEL(kemu_usb_dcd_edpt_close(&s_m, ep_addr));
}

void __wrap_dcd_edpt_close_all(uint8_t rhport)
{
    (void)rhport;
    WITH_MODEL(kemu_usb_dcd_edpt_close_all(&s_m));
}

bool __wrap_dcd_edpt_xfer(uint8_t rhport, uint8_t ep_addr, uint8_t *buffer, uint16_t total_bytes)
{
    (void)rhport;
    bool ok = false;
    WITH_MODEL(ok = kemu_usb_dcd_edpt_xfer(&s_m, ep_addr, buffer, total_bytes));
    return ok;
}

bool __wrap_dcd_edpt_xfer_fifo(uint8_t rhport, void *ff, uint16_t total_bytes)
{
    (void)rhport; (void)ff; (void)total_bytes;
    return false;           /* the controller has no dedicated hardware FIFOs */
}

void __wrap_dcd_edpt_stall(uint8_t rhport, uint8_t ep_addr)
{
    (void)rhport;
    WITH_MODEL(kemu_usb_dcd_edpt_stall(&s_m, ep_addr));
}

void __wrap_dcd_edpt_clear_stall(uint8_t rhport, uint8_t ep_addr)
{
    (void)rhport;
    WITH_MODEL(kemu_usb_dcd_edpt_clear_stall(&s_m, ep_addr));
}
