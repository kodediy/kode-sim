/**
 * @file usbdcd.c
 * @brief kemu_usbdcd.h: a TinyUSB device controller with the Mac as the bus.
 */
#include <stdlib.h>
#include <string.h>

#include "kemu_usbdcd.h"

struct kemu_usb_chunk {
    kemu_usb_chunk_t *next;
    uint32_t len, used;
    bool short_end;     /* the host ended its transfer here with a packet shorter than the endpoint's */
    uint8_t data[];
};

static void queue_ev(kemu_usb_t *m, kemu_usb_ev_kind_t kind)
{
    if (m->nev < KEMU_USB_EV_MAX) {
        memset(&m->ev[m->nev], 0, sizeof m->ev[0]);
        m->ev[m->nev].kind = kind;
        m->nev++;
    }
}

static void queue_xfer(kemu_usb_t *m, uint8_t ep_addr, uint32_t len, bool ok)
{
    if (m->nev < KEMU_USB_EV_MAX) {
        kemu_usb_ev_t *e = &m->ev[m->nev++];
        memset(e, 0, sizeof *e);
        e->kind = KEMU_USB_EV_XFER;
        e->ep_addr = ep_addr;
        e->len = len;
        e->ok = ok;
    }
}

static void drop_completions(kemu_usb_t *m, uint8_t ep_addr)
{
    uint32_t kept = 0;
    for (uint32_t i = 0; i < m->nev; i++) {
        if (m->ev[i].kind != KEMU_USB_EV_XFER || m->ev[i].ep_addr != ep_addr) {
            m->ev[kept++] = m->ev[i];
        }
    }
    m->nev = kept;
}

static kemu_usb_ep_t *ep_of(kemu_usb_t *m, uint8_t ep_addr)
{
    uint8_t const n = ep_addr & 0x7FU;
    if (n >= KEMU_USB_EPS) {
        return NULL;
    }
    return (ep_addr & 0x80U) ? &m->in[n] : &m->outp[n];
}

static void flush_queue(kemu_usb_ep_t *e)
{
    while (e->q_head != NULL) {
        kemu_usb_chunk_t *c = e->q_head;
        e->q_head = c->next;
        free(c);
    }
    e->q_tail = NULL;
    e->q_bytes = 0;
}

static void reset_ep(kemu_usb_ep_t *e)
{
    flush_queue(e);
    memset(e, 0, sizeof *e);
}

static void open_ep0(kemu_usb_t *m)
{
    m->in[0].open = m->outp[0].open = true;
    m->in[0].mps = m->outp[0].mps = 64;
}

void kemu_usb_init(kemu_usb_t *m, kemu_usb_out_t out, void *ctx)
{
    memset(m, 0, sizeof *m);
    m->out = out;
    m->ctx = ctx;
}

static void drop_all(kemu_usb_t *m);

static void try_attach(kemu_usb_t *m)
{
    if (m->inited && m->pullup && m->cable && !m->on_bus) {
        drop_all(m);
        open_ep0(m);
        m->nev = 0;
        m->on_bus = true;
        queue_ev(m, KEMU_USB_EV_BUS_RESET);
    }
}

static void drop_all(kemu_usb_t *m)
{
    for (uint32_t i = 0; i < KEMU_USB_EPS; i++) {
        reset_ep(&m->in[i]);
        reset_ep(&m->outp[i]);
    }
}

bool kemu_usb_dcd_init(kemu_usb_t *m)
{
    if (m->inited) { return false; }
    drop_all(m);
    open_ep0(m);
    m->nev = 0;
    m->inited = true;
    m->pullup = false;
    m->on_bus = false;
    return true;
}

void kemu_usb_dcd_deinit(kemu_usb_t *m)
{
    if (m->pullup) {
        kemu_usb_dcd_disconnect(m);
    }
    drop_all(m);
    m->nev = 0;
    m->on_bus = false;
    m->inited = false;
}

void kemu_usb_dcd_connect(kemu_usb_t *m)
{
    if (!m->inited || m->pullup) { return; }
    m->pullup = true;
    m->out.dev(m->ctx, KEMU_USB_DEV_PULLUP, 1, 0);
    try_attach(m);
}

void kemu_usb_dcd_disconnect(kemu_usb_t *m)
{
    if (!m->inited || !m->pullup) { return; }
    m->pullup = false;
    m->on_bus = false;
    drop_all(m);
    m->nev = 0;
    m->out.dev(m->ctx, KEMU_USB_DEV_PULLUP, 0, 0);
}

void kemu_usb_dcd_set_address(kemu_usb_t *m, uint8_t addr)
{
    if (!m->inited || !m->on_bus || addr > 127U) { return; }
    m->out.dev(m->ctx, KEMU_USB_DEV_ADDRESS, addr, 0);
    /* TinyUSB leaves the status stage of SET_ADDRESS to the driver (the DWC2 one sends its
     * zero-length IN right away, before the address takes): without it the host waits for ever. */
    (void)kemu_usb_dcd_edpt_xfer(m, 0x80U, NULL, 0U);
}

bool kemu_usb_dcd_edpt_open(kemu_usb_t *m, uint8_t ep_addr, uint16_t mps, uint8_t type)
{
    kemu_usb_ep_t *e = ep_of(m, ep_addr);
    if (!m->inited || e == NULL || mps == 0U || type > 3U ||
        mps > (type == 1U ? 1023U : 64U)) {
        return false;
    }
    drop_completions(m, ep_addr);
    reset_ep(e);
    e->open = true;
    e->mps = mps;
    e->type = type;
    m->out.dev(m->ctx, KEMU_USB_DEV_EP_OPEN, ep_addr, (uint16_t)(mps | ((uint16_t)type << 12)));
    return true;
}

void kemu_usb_dcd_edpt_close(kemu_usb_t *m, uint8_t ep_addr)
{
    kemu_usb_ep_t *e = ep_of(m, ep_addr);
    if (m->inited && e != NULL && (ep_addr & 0x7FU) != 0U) {
        drop_completions(m, ep_addr);
        reset_ep(e);
        m->out.dev(m->ctx, KEMU_USB_DEV_EP_CLOSE, ep_addr, 0);
    }
}

void kemu_usb_dcd_edpt_close_all(kemu_usb_t *m)
{
    if (!m->inited) { return; }
    for (uint32_t i = 1; i < KEMU_USB_EPS; i++) {
        drop_completions(m, (uint8_t)(i | 0x80U));
        drop_completions(m, (uint8_t)i);
        reset_ep(&m->in[i]);
        reset_ep(&m->outp[i]);
    }
    m->out.dev(m->ctx, KEMU_USB_DEV_EP_CLOSE, 0xFF, 0);
}

/* Hand the app's armed OUT transfer what the Mac has sent. It completes when it
 * has everything it asked for, or when the Mac's chunk ended with a short
 * packet (or a zero-length one), as an OUT transfer does on the bus. */
static void pump_out(kemu_usb_t *m, uint8_t num)
{
    kemu_usb_ep_t *e = &m->outp[num];
    uint32_t took = 0;
    bool ended = false;
    while (e->armed && !ended && e->q_head != NULL && (e->done < e->total || e->q_head->len == 0U)) {
        kemu_usb_chunk_t *c = e->q_head;
        uint32_t const room = e->total - e->done;
        uint32_t const n = (c->len - c->used < room) ? c->len - c->used : room;
        if (n != 0U) { memcpy(e->buf + e->done, c->data + c->used, n); }
        e->done += n;
        c->used += n;
        took += n;
        e->q_bytes -= n;
        if (c->used == c->len) {
            ended = c->short_end;
            e->q_head = c->next;
            if (e->q_head == NULL) {
                e->q_tail = NULL;
            }
            free(c);
        }
    }
    if (took != 0U) {
        m->out.ack(m->ctx, num, took);
    }
    if (e->armed && (e->done == e->total || ended)) {
        e->armed = false;
        queue_xfer(m, num, e->done, true);
    }
}

bool kemu_usb_dcd_edpt_xfer(kemu_usb_t *m, uint8_t ep_addr, uint8_t *buf, uint16_t total)
{
    kemu_usb_ep_t *e = ep_of(m, ep_addr);
    if (!m->inited || e == NULL || !e->open || e->stalled || e->armed ||
        (total != 0U && buf == NULL)) {
        return false;
    }
    uint8_t const num = ep_addr & 0x7FU;
    if (ep_addr & 0x80U) {
        /* IN: the Mac takes it as it is sent. Off the bus there is no host to take it. */
        if (!m->on_bus) {
            e->armed = true;
            e->buf = buf;
            e->total = total;
            e->done = 0;
            return true;
        }
        m->out.in_data(m->ctx, ep_addr, buf, total);
        queue_xfer(m, ep_addr, total, true);
        return true;
    }
    e->armed = true;
    e->buf = buf;
    e->total = total;
    e->done = 0;
    if (total == 0U) {
        e->armed = false;
        queue_xfer(m, ep_addr, 0, true);
        return true;
    }
    pump_out(m, num);
    return true;
}

void kemu_usb_dcd_edpt_stall(kemu_usb_t *m, uint8_t ep_addr)
{
    kemu_usb_ep_t *e = ep_of(m, ep_addr);
    if (m->inited && e != NULL && e->open) {
        e->armed = false;
        e->buf = NULL;
        flush_queue(e);
        e->stalled = true;
        m->out.dev(m->ctx, KEMU_USB_DEV_STALL, ep_addr, 0);
    }
}

void kemu_usb_dcd_edpt_clear_stall(kemu_usb_t *m, uint8_t ep_addr)
{
    kemu_usb_ep_t *e = ep_of(m, ep_addr);
    if (m->inited && e != NULL && e->open) {
        e->stalled = false;
        m->out.dev(m->ctx, KEMU_USB_DEV_CLEAR, ep_addr, 0);
    }
}

/* ---- the Mac's side ---- */

void kemu_usb_bus(kemu_usb_t *m, uint8_t kind)
{
    switch (kind) {
    case KEMU_USB_BUS_CABLE_IN:
        m->cable = true;
        try_attach(m);
        break;
    case KEMU_USB_BUS_CABLE_OUT:
        m->cable = false;
        if (m->on_bus) {
            m->on_bus = false;
            drop_all(m);
            open_ep0(m);
            m->nev = 0;
            queue_ev(m, KEMU_USB_EV_UNPLUGGED);
        }
        break;
    case KEMU_USB_BUS_RESET:
        if (m->inited && m->on_bus) {
            drop_all(m);
            open_ep0(m);
            m->nev = 0;
            queue_ev(m, KEMU_USB_EV_BUS_RESET);
        }
        break;
    case KEMU_USB_BUS_SUSPEND:
        if (m->on_bus) {
            queue_ev(m, KEMU_USB_EV_SUSPEND);
        }
        break;
    case KEMU_USB_BUS_RESUME:
        if (m->on_bus) {
            queue_ev(m, KEMU_USB_EV_RESUME);
        }
        break;
    default:
        break;
    }
}

void kemu_usb_setup(kemu_usb_t *m, const uint8_t setup[8])
{
    if (!m->on_bus) {
        return;
    }
    drop_completions(m, 0x80U);
    drop_completions(m, 0U);
    if (m->nev >= KEMU_USB_EV_MAX) {
        return;
    }
    /* A SETUP aborts whatever the control pipe was doing and clears its stall. */
    for (uint32_t d = 0; d < 2; d++) {
        kemu_usb_ep_t *e = d ? &m->in[0] : &m->outp[0];
        e->armed = false;
        e->stalled = false;
    }
    flush_queue(&m->outp[0]);
    kemu_usb_ev_t *e = &m->ev[m->nev++];
    memset(e, 0, sizeof *e);
    e->kind = KEMU_USB_EV_SETUP;
    memcpy(e->setup, setup, 8);
}

bool kemu_usb_out_data(kemu_usb_t *m, uint8_t ep, const uint8_t *p, uint32_t len)
{
    if (!m->inited || ep >= KEMU_USB_EPS || !m->outp[ep].open ||
        m->outp[ep].stalled || !m->on_bus || (len != 0U && p == NULL)) {
        return false;
    }
    kemu_usb_ep_t *e = &m->outp[ep];
    if (len > KEMU_USB_CREDIT - e->q_bytes) { return false; }
    kemu_usb_chunk_t *c = malloc(sizeof *c + len);
    if (c == NULL) {
        return false;
    }
    c->next = NULL;
    c->len = len;
    c->used = 0;
    c->short_end = len == 0U || (e->mps != 0U && (len % e->mps) != 0U);
    if (len != 0U) { memcpy(c->data, p, len); }
    if (e->q_tail != NULL) {
        e->q_tail->next = c;
    } else {
        e->q_head = c;
    }
    e->q_tail = c;
    e->q_bytes += len;
    pump_out(m, ep);
    return true;
}

uint32_t kemu_usb_take_events(kemu_usb_t *m, kemu_usb_ev_t *dst, uint32_t cap)
{
    uint32_t const n = m->nev < cap ? m->nev : cap;
    memcpy(dst, m->ev, n * sizeof dst[0]);
    memmove(m->ev, &m->ev[n], (m->nev - n) * sizeof m->ev[0]);
    m->nev -= n;
    return n;
}
