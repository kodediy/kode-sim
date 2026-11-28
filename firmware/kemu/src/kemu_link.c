/**
 * @file kemu_link.c
 * @brief UART1 to the Mac, and the hook that starts it before the app does.
 *
 * kemu is started by wrapping app_main itself: __wrap_app_main brings the
 * link up and then calls the app's own. That runs on IDF's main task with
 * the scheduler up, before kode_init() can reach a single driver.
 *
 * UART1 because esp-emu bridges out only UART0 (--uart-tcp) and UART1
 * (--uart1-tcp). Neither is free on a Kode Dot: kode-sdk lends both to apps
 * for the expansion header, UART0 first (KBSP_EXP_CTRL). UART1 is the one
 * an app reaches second, and UART0 also carries the ROM's and the
 * bootloader's output. An app's second expansion UART is therefore refused
 * in the simulator alone (README, "What is simulated").
 */
#include <string.h>

#include "driver/uart.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

#include "kbsp_board.h"
#include "kemu_clock.h"
#include "kemu_console.h"
#include "kemu_link.h"

#define LINK_UART      UART_NUM_1
#define LINK_RX_RING   16384
#define LINK_FRAME_MAX 8192    /* the largest frame from the Mac: a PCM_IN chunk */
#define CLOCK_WAIT_MS  1000    /* the Mac's answer to HELLO takes a few ms */

static SemaphoreHandle_t s_tx;
static kemu_rx_cb_t s_handlers[256];
static uint8_t s_rx_frame[LINK_FRAME_MAX];
static kemu_dec_t s_dec;

void kemu_on(uint8_t type, kemu_rx_cb_t cb)
{
    s_handlers[type] = cb;
}

void kemu_send2(uint8_t type, const void *head, uint32_t head_len,
                const void *body, uint32_t body_len)
{
    uint8_t h[KEMU_FRAME_HEADER];
    (void)kemu_frame_header(h, type, head_len + body_len);
    xSemaphoreTake(s_tx, portMAX_DELAY);
    uart_write_bytes(LINK_UART, h, sizeof h);
    if (head_len != 0U) {
        uart_write_bytes(LINK_UART, head, head_len);
    }
    if (body_len != 0U) {
        uart_write_bytes(LINK_UART, body, body_len);
    }
    xSemaphoreGive(s_tx);
}

void kemu_send(uint8_t type, const void *p, uint32_t len)
{
    kemu_send2(type, p, len, NULL, 0U);
}

static void dispatch(uint8_t type, const uint8_t *p, uint32_t len, void *arg)
{
    (void)arg;
    if (s_handlers[type] != NULL) {
        s_handlers[type](p, len);
    }
}

/* A frame is handed on as soon as its last byte is in. uart_read_bytes()
 * waits until it has every byte asked for or the timeout has passed, so a
 * read of a chunk's worth held each short frame - an ack, a HELLO's answer,
 * an SD block - for the whole timeout. So: wait for one byte, then take
 * whatever else the driver already has, without waiting. */
static void rx_task(void *arg)
{
    static uint8_t chunk[1024];
    (void)arg;
    for (;;) {
        int n = uart_read_bytes(LINK_UART, chunk, 1U, portMAX_DELAY);
        if (n <= 0) {
            continue;
        }
        size_t more = 0U;
        if ((uart_get_buffered_data_len(LINK_UART, &more) == ESP_OK) && (more != 0U)) {
            more = (more < sizeof chunk - 1U) ? more : (sizeof chunk - 1U);
            int const m = uart_read_bytes(LINK_UART, &chunk[1], (uint32_t)more, 0);
            n += (m > 0) ? m : 0;
        }
        (void)kemu_dec_feed(&s_dec, chunk, (size_t)n, dispatch, NULL);
    }
}

void kemu_link_start(void)
{
    const uart_config_t cfg = {
        .baud_rate  = 115200,     /* the emulator ignores it */
        .data_bits  = UART_DATA_8_BITS,
        .parity     = UART_PARITY_DISABLE,
        .stop_bits  = UART_STOP_BITS_1,
        .flow_ctrl  = UART_HW_FLOWCTRL_DISABLE,
        .source_clk = UART_SCLK_DEFAULT,
    };
    s_tx = xSemaphoreCreateMutex();
    kemu_dec_init(&s_dec, s_rx_frame, sizeof s_rx_frame);
    /* No TX ring: uart_write_bytes then copies straight into the FIFO,
     * which the emulator drains as fast as it is filled. No pins either:
     * the board's own GPIOs stay the drivers'. */
    ESP_ERROR_CHECK(uart_driver_install(LINK_UART, LINK_RX_RING, 0, 0, NULL, 0));
    ESP_ERROR_CHECK(uart_param_config(LINK_UART, &cfg));
    xTaskCreate(rx_task, "kemu_rx", 4096, NULL, configMAX_PRIORITIES - 2, NULL);

    const uint16_t hello[3] = {KEMU_PROTO_VERSION, KBSP_LCD_H_RES, KBSP_LCD_V_RES};
    kemu_send(KEMU_T_HELLO, hello, sizeof hello);
}

extern void __real_app_main(void);

void __wrap_app_main(void)
{
    kemu_console_start();
    kemu_clock_start();
    kemu_link_start();
    kemu_clock_wait(CLOCK_WAIT_MS);
    __real_app_main();
}
