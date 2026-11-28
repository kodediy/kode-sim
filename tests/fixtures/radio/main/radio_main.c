/* SPDX-License-Identifier: GPL-3.0-or-later */
/*
 * A kodeOS app that wants WiFi, for kode-emu's tests: it asks the radio the
 * way an app does, logs every answer, and once it has an address serves a
 * TCP echo on ECHO_PORT - what a server app (KodeCraft's 25565) does. Once
 * the radio has settled, up or failed, it mounts the microSD and asks about
 * it, as an app that keeps its files on the card does: since kode-sdk's
 * #313 an app's card is mounted when it is first used, and the card and the
 * C5's link share the SDMMC controller on two slots, so the card mounts
 * with the link up. With the network up it says what TOP's pad is - a
 * GPIO, an input, or something the network took - and it logs every button
 * edge, so that a test can see the buttons still work with the network up.
 */
#include "kode_sys.h"
#include "kaos.h"     /* radio_state_request() and the rest, below the facade */
#include "driver/gpio.h"
#include "kbsp_pins.h"
#include "lwip/sockets.h"

#define TAG "radio"
#define ECHO_PORT 7777

KODE_APP_MANIFEST("Radio", "test");

static QEvtPtr     l_queue[KQP_AO_QUEUE_LEN];
static StackType_t l_stack[KQP_AO_STACK_BYTES / sizeof(StackType_t)];
static QActive     l_ao;
static bool        s_serving;

static void echo_task(void *arg)
{
    (void)arg;
    int const s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(ECHO_PORT) };
    if ((s < 0) || (bind(s, (struct sockaddr *)&a, sizeof a) != 0) || (listen(s, 1) != 0)) {
        KLOG_E(TAG, "echo: no socket");
        vTaskDelete(NULL);
        return;
    }
    KLOG_I(TAG, "echo listening on %d", ECHO_PORT);
    for (;;) {
        int const c = accept(s, NULL, NULL);
        char buf[64];
        int n;
        while ((n = recv(c, buf, sizeof buf, 0)) > 0) {
            (void)send(c, buf, (size_t)n, 0);
        }
        close(c);
    }
}

static QState Radio_listening(QActive * const me, QEvt const * const e)
{
    (void)me;
    switch (e->sig) {
        case RADIO_STATE_SIG: {
            RadioStateEvt const * const r = (RadioStateEvt const *)e;
            KLOG_I(TAG, "radio link=%u", (unsigned)r->link);
            if ((r->link == (uint8_t)KRADIO_UP) || (r->link == (uint8_t)KRADIO_FAILED)) {
                /* Posted in order to AO_Storage: the mount runs first. */
                storage_request(KDRV_MEDIUM_SD, STORAGE_OP_MOUNT);
                storage_request(KDRV_MEDIUM_SD, STORAGE_OP_INFO);
            }
            return Q_HANDLED();
        }
        case STORAGE_INFO_SIG: {
            StorageInfoEvt const * const i = (StorageInfoEvt const *)e;
            if (i->medium == (uint8_t)KDRV_MEDIUM_SD) {
                KLOG_I(TAG, "sd mounted=%u card_mb=%lu", (unsigned)i->mounted,
                       (unsigned long)i->card_mb);
            }
            return Q_HANDLED();
        }
        case WIFI_STATE_SIG: {
            WifiStateEvt const * const w = (WifiStateEvt const *)e;
            uint8_t const *const ip = (uint8_t const *)&w->ip4;
            KLOG_I(TAG, "wifi state=%u ip=%u.%u.%u.%u enabled=%u ssid=%s", (unsigned)w->state,
                   ip[0], ip[1], ip[2], ip[3], (unsigned)w->enabled, w->ssid);
            if ((w->ip4 != 0U) && !s_serving) {
                gpio_io_config_t io;
                if (gpio_get_io_config((gpio_num_t)KBSP_PIN_BUTTON_TOP, &io) == ESP_OK) {
                    KLOG_I(TAG, "top pad fun_sel=%u ie=%u oe=%u", (unsigned)io.fun_sel,
                           (unsigned)io.ie, (unsigned)io.oe);
                }
                s_serving = true;
                xTaskCreate(echo_task, "echo", 4096, NULL, 5, NULL);
            }
            return Q_HANDLED();
        }
        case BUTTON_EVT_SIG: {
            ButtonEvt const * const b = (ButtonEvt const *)e;
            KLOG_I(TAG, "button id=%u pressed=%u", (unsigned)b->id, (unsigned)b->pressed);
            return Q_HANDLED();
        }
        default:
            return Q_SUPER(&QHsm_top);
    }
}

static QState Radio_initial(QActive * const me, void const * const par)
{
    (void)par;
    QActive_subscribe(me, RADIO_STATE_SIG);
    QActive_subscribe(me, WIFI_STATE_SIG);
    QActive_subscribe(me, STORAGE_INFO_SIG);
    QActive_subscribe(me, BUTTON_EVT_SIG);
    radio_state_request();
    wifi_start_request();
    wifi_fast_request(true);
    wifi_state_request();
    KLOG_I(TAG, "up; asked for the radio and for WiFi");
    return Q_TRAN(&Radio_listening);
}

void app_main(void)
{
    kode_init();
    QActive_ctor(&l_ao, Q_STATE_CAST(&Radio_initial));
    QActive_setAttr(&l_ao, TASK_NAME_ATTR, "radio");
    QACTIVE_START(&l_ao, KODE_PRIO_USER_LOW, l_queue, Q_DIM(l_queue),
                  l_stack, sizeof(l_stack), (void *)0);
}
