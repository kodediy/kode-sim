/* The USB device controller model, driven the way TinyUSB and the Mac drive it. */
#include <string.h>

#include "check.h"
#include "kemu_usbdcd.h"

typedef struct {
    uint8_t in_ep[8];
    uint32_t in_len[8];
    uint8_t in_first[8];
    uint32_t n_in;
    uint32_t n_dev;
    uint8_t dev_kind[16], dev_a[16];
    uint32_t acked;
} log_t;

static void on_in(void *c, uint8_t ep, const uint8_t *p, uint32_t len)
{
    log_t *l = c;
    if (l->n_in < 8) {
        l->in_ep[l->n_in] = ep;
        l->in_len[l->n_in] = len;
        l->in_first[l->n_in] = len ? p[0] : 0;
    }
    l->n_in++;
}

static void on_dev(void *c, uint8_t kind, uint8_t a, uint16_t b)
{
    log_t *l = c;
    (void)b;
    if (l->n_dev < 16) {
        l->dev_kind[l->n_dev] = kind;
        l->dev_a[l->n_dev] = a;
    }
    l->n_dev++;
}

static void on_ack(void *c, uint8_t ep, uint32_t bytes)
{
    log_t *l = c;
    (void)ep;
    l->acked += bytes;
}

static log_t lg;
static kemu_usb_t m;

static void fresh(void)
{
    if (m.inited) { kemu_usb_dcd_deinit(&m); }
    memset(&lg, 0, sizeof lg);
    kemu_usb_init(&m, (kemu_usb_out_t){on_in, on_dev, on_ack}, &lg);
}

static uint32_t drain(kemu_usb_ev_t *ev)
{
    return kemu_usb_take_events(&m, ev, KEMU_USB_EV_MAX);
}

static void bring_up(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    fresh();
    CHECK(kemu_usb_dcd_init(&m));
    kemu_usb_dcd_connect(&m);
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_IN);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_BUS_RESET);
}

static void test_the_bus_resets_once_the_cable_and_the_pullup_are_both_there(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    fresh();
    CHECK(kemu_usb_dcd_init(&m));
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_IN);
    CHECK(drain(ev) == 0);                      /* a cable, and a device that has not announced itself */
    kemu_usb_dcd_connect(&m);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_BUS_RESET);
    CHECK(lg.n_dev == 1 && lg.dev_kind[0] == KEMU_USB_DEV_PULLUP && lg.dev_a[0] == 1);

    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_OUT);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_UNPLUGGED);
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_OUT);
    CHECK(drain(ev) == 0);                      /* unplugging twice is one event */
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_IN);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_BUS_RESET);   /* and back in: the host resets the bus again */

    kemu_usb_dcd_disconnect(&m);
    kemu_usb_setup(&m, (const uint8_t[8]){0x80, 6, 0, 1, 0, 0, 18, 0});
    CHECK(drain(ev) == 0);                      /* no pull-up: nobody on the bus to talk */
}

static void test_a_control_read_goes_out_whole_and_its_status_completes_at_once(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t desc[18] = {18, 1, 0, 2};
    bring_up();
    kemu_usb_setup(&m, (const uint8_t[8]){0x80, 6, 0, 1, 0, 0, 18, 0});
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_SETUP && ev[0].setup[1] == 6 && ev[0].setup[6] == 18);
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x80, desc, 18));
    CHECK(lg.n_in == 1 && lg.in_ep[0] == 0x80 && lg.in_len[0] == 18 && lg.in_first[0] == 18);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_XFER && ev[0].ep_addr == 0x80 && ev[0].len == 18 && ev[0].ok);
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x00, NULL, 0));   /* the status stage */
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_XFER && ev[0].ep_addr == 0x00 && ev[0].len == 0);
}

static void test_a_control_write_takes_the_data_the_host_sent_after_its_setup(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t buf[16];
    bring_up();
    kemu_usb_setup(&m, (const uint8_t[8]){0x21, 9, 0, 2, 0, 0, 1, 0});     /* SET_REPORT, one byte */
    CHECK(drain(ev) == 1);
    CHECK(kemu_usb_out_data(&m, 0, (const uint8_t[]){0x05}, 1));           /* it comes before the app asks */
    CHECK(drain(ev) == 0);
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x00, buf, 1));
    CHECK(buf[0] == 5 && lg.acked == 1);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_XFER && ev[0].len == 1);
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x80, NULL, 0));                      /* status, IN, zero bytes */
    CHECK(lg.n_in == 1 && lg.in_ep[0] == 0x80 && lg.in_len[0] == 0);
}

static void test_a_new_setup_aborts_the_control_pipe(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t buf[8];
    bring_up();
    kemu_usb_setup(&m, (const uint8_t[8]){0x21, 9, 0, 2, 0, 0, 8, 0});
    drain(ev);
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x00, buf, 8));      /* armed, waiting for data that never comes */
    kemu_usb_dcd_edpt_stall(&m, 0x00);
    kemu_usb_setup(&m, (const uint8_t[8]){0x80, 8, 0, 0, 0, 0, 1, 0});
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_SETUP);
    CHECK(kemu_usb_out_data(&m, 0, (const uint8_t[]){1}, 1));
    CHECK(drain(ev) == 0);                                /* the old armed transfer is gone: the byte waits */
}

static void test_a_bulk_out_transfer_completes_when_full_or_when_the_host_ends_it_short(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    static uint8_t data[4096], buf[4096];
    for (size_t i = 0; i < sizeof data; i++) { data[i] = (uint8_t)i; }
    bring_up();
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x02, 64, 2));
    CHECK(lg.dev_kind[lg.n_dev - 1] == KEMU_USB_DEV_EP_OPEN && lg.dev_a[lg.n_dev - 1] == 0x02);

    /* a 31-byte CBW is a short packet: it ends a transfer armed for more */
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x02, buf, 512));
    CHECK(drain(ev) == 0);
    CHECK(kemu_usb_out_data(&m, 2, data, 31));
    CHECK(drain(ev) == 1 && ev[0].ep_addr == 0x02 && ev[0].len == 31 && ev[0].ok);
    CHECK(memcmp(buf, data, 31) == 0);

    /* 4096 bytes in two chunks of 2048: whole packets, so it goes on until full */
    lg.acked = 0;
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x02, buf, 4096));
    CHECK(kemu_usb_out_data(&m, 2, data, 2048));
    CHECK(drain(ev) == 0 && lg.acked == 2048);
    CHECK(kemu_usb_out_data(&m, 2, data + 2048, 2048));
    CHECK(drain(ev) == 1 && ev[0].len == 4096);
    CHECK(memcmp(buf, data, 4096) == 0 && lg.acked == 4096);

    /* data first, the app arms later and in two bites: one chunk serves both */
    CHECK(kemu_usb_out_data(&m, 2, data, 1024));
    CHECK(drain(ev) == 0);
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x02, buf, 512));
    CHECK(drain(ev) == 1 && ev[0].len == 512);
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x02, buf, 512));
    CHECK(drain(ev) == 1 && ev[0].len == 512 && memcmp(buf, data + 512, 512) == 0);

    /* nothing asked for, nothing to wait for */
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x02, NULL, 0));
    CHECK(drain(ev) == 1 && ev[0].len == 0);
    CHECK(!kemu_usb_out_data(&m, 5, data, 4));            /* an endpoint that is not open */
}

static void test_in_data_goes_out_at_once_and_waits_off_the_bus(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t rep[8] = {0, 0, 4, 0, 0, 0, 0, 0};
    fresh();
    CHECK(kemu_usb_dcd_init(&m));
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x81, 8, 3));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x81, rep, 8));
    CHECK(lg.n_in == 0 && drain(ev) == 0);                /* no host: it stays armed */
    kemu_usb_dcd_connect(&m);
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_IN);
    CHECK(drain(ev) == 1);                                /* the bus reset: the stack re-arms from scratch */
    CHECK(!m.in[1].armed && !m.in[1].open);
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x81, 8, 3));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x81, rep, 8));
    CHECK(lg.n_in == 1 && lg.in_ep[0] == 0x81 && lg.in_len[0] == 8 && lg.in_first[0] == 0);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_XFER && ev[0].ep_addr == 0x81 && ev[0].len == 8);
}

static void test_stalls_and_suspends_reach_both_sides(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    bring_up();
    kemu_usb_dcd_edpt_stall(&m, 0x80);
    CHECK(lg.dev_kind[lg.n_dev - 1] == KEMU_USB_DEV_STALL && lg.dev_a[lg.n_dev - 1] == 0x80);
    kemu_usb_dcd_edpt_clear_stall(&m, 0x80);
    CHECK(lg.dev_kind[lg.n_dev - 1] == KEMU_USB_DEV_CLEAR);
    kemu_usb_bus(&m, KEMU_USB_BUS_SUSPEND);
    kemu_usb_bus(&m, KEMU_USB_BUS_RESUME);
    CHECK(drain(ev) == 2 && ev[0].kind == KEMU_USB_EV_SUSPEND && ev[1].kind == KEMU_USB_EV_RESUME);
    kemu_usb_dcd_set_address(&m, 7);
    CHECK(lg.dev_kind[lg.n_dev - 1] == KEMU_USB_DEV_ADDRESS && lg.dev_a[lg.n_dev - 1] == 7);
    /* ... and its status stage, which the stack leaves to the driver */
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_XFER && ev[0].ep_addr == 0x80U && ev[0].len == 0U);
}

static void test_a_stack_that_stops_leaves_the_bus_and_what_waited_is_dropped(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    fresh();
    CHECK(kemu_usb_dcd_init(&m));
    kemu_usb_dcd_connect(&m);
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_IN);        /* a bus reset is queued, and the stack stops before it is told */
    kemu_usb_dcd_deinit(&m);
    CHECK(lg.dev_kind[lg.n_dev - 1] == KEMU_USB_DEV_PULLUP && lg.dev_a[lg.n_dev - 1] == 0);
    CHECK(drain(ev) == 0);
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_OUT);       /* the cable pulled out later tells a stack that is gone nothing */
    CHECK(drain(ev) == 0);
    CHECK(kemu_usb_dcd_init(&m));                   /* and a stack that starts again finds the cable and the bus anew */
    kemu_usb_dcd_connect(&m);
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_IN);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_BUS_RESET);
}

static void test_invalid_states_and_repeated_initialization(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t data[64] = {0};
    fresh();
    kemu_usb_dcd_connect(&m);
    CHECK(!m.pullup && lg.n_dev == 0);
    CHECK(!kemu_usb_dcd_edpt_open(&m, 0x81, 8, 3));
    CHECK(!kemu_usb_dcd_edpt_xfer(&m, 0x80, data, sizeof data));
    CHECK(kemu_usb_dcd_init(&m));
    CHECK(!kemu_usb_dcd_init(&m));
    CHECK(!kemu_usb_dcd_edpt_open(&m, KEMU_USB_EPS, 64, 2));
    CHECK(!kemu_usb_dcd_edpt_open(&m, 1, 0, 2));
    CHECK(!kemu_usb_dcd_edpt_open(&m, 1, 512, 2));
    CHECK(!kemu_usb_dcd_edpt_open(&m, 1, 64, 4));
    CHECK(!kemu_usb_dcd_edpt_xfer(&m, 0x80, NULL, 1));
    kemu_usb_dcd_connect(&m);
    kemu_usb_dcd_connect(&m);
    CHECK(lg.n_dev == 1);
    kemu_usb_dcd_deinit(&m);
    uint32_t const sent = lg.n_dev;
    kemu_usb_dcd_deinit(&m);
    kemu_usb_dcd_connect(&m);
    kemu_usb_dcd_set_address(&m, 1);
    kemu_usb_dcd_edpt_stall(&m, 0x80);
    kemu_usb_dcd_edpt_clear_stall(&m, 0x80);
    kemu_usb_dcd_edpt_close_all(&m);
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_IN);
    kemu_usb_bus(&m, KEMU_USB_BUS_SUSPEND);
    kemu_usb_bus(&m, KEMU_USB_BUS_RESUME);
    kemu_usb_setup(&m, (uint8_t[8]){0});
    CHECK(!kemu_usb_out_data(&m, 0, data, sizeof data));
    CHECK(!m.in[0].open && !m.outp[0].open);
    CHECK(lg.n_dev == sent && drain(ev) == 0);
    CHECK(kemu_usb_dcd_init(&m));
    kemu_usb_dcd_connect(&m);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_BUS_RESET);
    kemu_usb_dcd_deinit(&m);
}

static void test_reconnect_discards_endpoints_buffers_and_old_completions(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t data[64] = {0}, dst[64];
    bring_up();
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x01, 64, 2));
    CHECK(kemu_usb_out_data(&m, 1, data, sizeof data));
    CHECK(m.outp[1].q_bytes == sizeof data);
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x82, 8, 3));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x82, data, 8));
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_OUT);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_UNPLUGGED);
    CHECK(!m.outp[1].open && m.outp[1].q_head == NULL);
    kemu_usb_bus(&m, KEMU_USB_BUS_CABLE_IN);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_BUS_RESET);
    CHECK(m.in[0].open && m.outp[0].open);
    CHECK(!kemu_usb_dcd_edpt_xfer(&m, 1, dst, sizeof dst));
    CHECK(kemu_usb_dcd_edpt_open(&m, 1, 64, 2));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 1, dst, sizeof dst));
    CHECK(drain(ev) == 0);
    kemu_usb_dcd_deinit(&m);
    CHECK(m.outp[1].buf == NULL && m.outp[1].q_head == NULL);
}

static void test_stall_aborts_transfers_and_out_credit_is_bounded(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t data[KEMU_USB_CREDIT] = {0}, dst[64];
    bring_up();
    CHECK(kemu_usb_dcd_edpt_open(&m, 1, 64, 2));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 1, dst, sizeof dst));
    CHECK(!kemu_usb_dcd_edpt_xfer(&m, 1, dst, sizeof dst));
    kemu_usb_dcd_edpt_stall(&m, 1);
    CHECK(!m.outp[1].armed && m.outp[1].buf == NULL);
    CHECK(!kemu_usb_out_data(&m, 1, data, 1));
    CHECK(!kemu_usb_dcd_edpt_xfer(&m, 1, dst, sizeof dst));
    kemu_usb_dcd_edpt_clear_stall(&m, 1);
    CHECK(kemu_usb_out_data(&m, 1, data, sizeof data));
    CHECK(!kemu_usb_out_data(&m, 1, data, 1));
    CHECK(!kemu_usb_out_data(&m, 1, NULL, 1));
    CHECK(drain(ev) == 0);
    kemu_usb_dcd_deinit(&m);
    CHECK(m.outp[1].q_bytes == 0 && m.outp[1].q_head == NULL);
}

static void test_bus_reset_cancels_configured_endpoints_and_pending_events(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t data[64] = {0};
    bring_up();
    CHECK(kemu_usb_dcd_edpt_open(&m, 1, 64, 2));
    CHECK(kemu_usb_out_data(&m, 1, data, sizeof data));
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x81, 8, 3));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x81, data, 8));
    kemu_usb_bus(&m, KEMU_USB_BUS_RESET);
    CHECK(m.cable && m.pullup && m.on_bus);
    CHECK(!m.outp[1].open && m.outp[1].q_head == NULL && !m.in[1].open);
    CHECK(m.in[0].open && m.outp[0].open);
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_BUS_RESET);
    kemu_usb_setup(&m, (uint8_t[8]){0x80, 6, 0, 1, 0, 0, 18, 0});
    CHECK(drain(ev) == 1 && ev[0].kind == KEMU_USB_EV_SETUP);
    kemu_usb_dcd_deinit(&m);
    kemu_usb_bus(&m, KEMU_USB_BUS_RESET);
    CHECK(drain(ev) == 0 && !m.in[0].open);
}

static void test_endpoint_close_removes_only_its_pending_completions(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t data[8] = {0};
    bring_up();
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x81, sizeof data, 3));
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x82, sizeof data, 3));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x81, data, 1));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x82, data, 2));
    kemu_usb_bus(&m, KEMU_USB_BUS_SUSPEND);
    kemu_usb_dcd_edpt_close(&m, 0x81);
    CHECK(!kemu_usb_dcd_edpt_xfer(&m, 0x81, data, 3));
    CHECK(drain(ev) == 2);
    CHECK(ev[0].kind == KEMU_USB_EV_XFER && ev[0].ep_addr == 0x82 && ev[0].len == 2);
    CHECK(ev[1].kind == KEMU_USB_EV_SUSPEND);
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x81, sizeof data, 3));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x81, data, 3));
    CHECK(drain(ev) == 1 && ev[0].ep_addr == 0x81 && ev[0].len == 3);
}

static void test_closing_all_endpoints_keeps_only_control_completions(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t data[8] = {0};
    bring_up();
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x81, sizeof data, 3));
    CHECK(kemu_usb_dcd_edpt_open(&m, 1, sizeof data, 2));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x80, data, 1));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x81, data, 2));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 1, data, 3));
    CHECK(kemu_usb_out_data(&m, 1, data, 3));
    kemu_usb_dcd_edpt_close_all(&m);
    CHECK(drain(ev) == 1 && ev[0].ep_addr == 0x80 && ev[0].len == 1);
    CHECK(m.in[0].open && m.outp[0].open);
    CHECK(!m.in[1].open && !m.outp[1].open);
}

static void test_reopening_an_endpoint_cancels_its_old_completion(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t data[8] = {0};
    bring_up();
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x81, sizeof data, 3));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x81, data, 1));
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x81, sizeof data, 3));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x81, data, 2));
    CHECK(drain(ev) == 1 && ev[0].ep_addr == 0x81 && ev[0].len == 2);
}

static void test_new_setup_cancels_old_control_completions_only(void)
{
    kemu_usb_ev_t ev[KEMU_USB_EV_MAX];
    uint8_t data[8] = {0};
    uint8_t setup[8] = {0x80, 8, 0, 0, 0, 0, 1, 0};
    bring_up();
    CHECK(kemu_usb_dcd_edpt_open(&m, 0x81, sizeof data, 3));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x80, data, 1));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0, NULL, 0));
    CHECK(kemu_usb_dcd_edpt_xfer(&m, 0x81, data, 2));
    kemu_usb_setup(&m, setup);
    CHECK(drain(ev) == 2);
    CHECK(ev[0].kind == KEMU_USB_EV_XFER && ev[0].ep_addr == 0x81);
    CHECK(ev[1].kind == KEMU_USB_EV_SETUP && memcmp(ev[1].setup, setup, sizeof setup) == 0);
}

int main(void)
{
    RUN(test_the_bus_resets_once_the_cable_and_the_pullup_are_both_there);
    RUN(test_a_control_read_goes_out_whole_and_its_status_completes_at_once);
    RUN(test_a_control_write_takes_the_data_the_host_sent_after_its_setup);
    RUN(test_a_new_setup_aborts_the_control_pipe);
    RUN(test_a_bulk_out_transfer_completes_when_full_or_when_the_host_ends_it_short);
    RUN(test_in_data_goes_out_at_once_and_waits_off_the_bus);
    RUN(test_stalls_and_suspends_reach_both_sides);
    RUN(test_a_stack_that_stops_leaves_the_bus_and_what_waited_is_dropped);
    RUN(test_invalid_states_and_repeated_initialization);
    RUN(test_reconnect_discards_endpoints_buffers_and_old_completions);
    RUN(test_stall_aborts_transfers_and_out_credit_is_bounded);
    RUN(test_bus_reset_cancels_configured_endpoints_and_pending_events);
    RUN(test_endpoint_close_removes_only_its_pending_completions);
    RUN(test_closing_all_endpoints_keeps_only_control_completions);
    RUN(test_reopening_an_endpoint_cancels_its_old_completion);
    RUN(test_new_setup_cancels_old_control_completions_only);
    CHECK_DONE();
}
