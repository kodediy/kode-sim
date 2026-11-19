/**
 * @file kemu_usb.c
 * @brief Full-speed USB device hardware, with kode-sim as its host.
 *
 * ESP-TinyUSB and applications that own TinyUSB run the same stack. Only
 * the PHY and DCD are virtual. Events use the installed TinyUSB headers.
 */
#include <stdlib.h>
#include <string.h>

#include "esp_err.h"
#include "esp_log.h"
#include "esp_private/usb_phy.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

#include "kemu_link.h"
#include "kemu_usb.h"
#include "kemu_usb_internal.h"
#include "kemu_usbdcd.h"

#ifdef KEMU_USB_TINYUSB
#include "tusb.h"
#include "device/dcd.h"

/* An installed but unused TinyUSB component must not pull its stack and
 * descriptor callbacks into an application solely because kemu is present. */
#pragma weak dcd_event_handler

/* The linker does not type-check --wrap targets. Make ABI drift a compile
 * error, including TinyUSB 0.21's new transfer context argument. */
#define CHECK_WRAP(sym) extern __typeof__(sym) __wrap_ ## sym
CHECK_WRAP(dcd_init);
CHECK_WRAP(dcd_int_enable);
CHECK_WRAP(dcd_int_disable);
CHECK_WRAP(dcd_set_address);
CHECK_WRAP(dcd_edpt_open);
CHECK_WRAP(dcd_edpt_xfer);
CHECK_WRAP(dcd_edpt_xfer_fifo);
CHECK_WRAP(dcd_edpt_stall);
CHECK_WRAP(dcd_edpt_clear_stall);
#undef CHECK_WRAP
#endif

static kemu_usb_t s_m;
static StaticSemaphore_t s_model_storage, s_delivery_storage, s_stack_storage, s_driver_storage;
static SemaphoreHandle_t s_lock, s_delivery, s_stack, s_driver;
static portMUX_TYPE s_once = portMUX_INITIALIZER_UNLOCKED;
static bool s_up;
static bool s_ints;
static bool s_stopping;
static uint32_t s_mask_depth;
static TaskHandle_t s_consumer;
static void *s_phy;
#ifdef KEMU_USB_TINYUSB
static TaskHandle_t s_events;
static uint8_t s_rhport;
static QueueHandle_t s_event_queue;
static const dcd_event_t *s_sending_event;
#endif

static void m_in(void *ctx, uint8_t ep_addr, const uint8_t *p, uint32_t len)
{
    (void)ctx;
    kemu_send2(KEMU_T_USB_IN, &ep_addr, 1U, p, len);
}

static void m_dev(void *ctx, uint8_t kind, uint8_t a, uint16_t b)
{
    (void)ctx;
    uint8_t const f[4] = {kind, a, (uint8_t)b, (uint8_t)(b >> 8)};
    kemu_send(KEMU_T_USB_DEV, f, sizeof f);
}

static void m_ack(void *ctx, uint8_t ep, uint32_t bytes)
{
    (void)ctx;
    uint8_t const f[5] = {ep, (uint8_t)bytes, (uint8_t)(bytes >> 8), (uint8_t)(bytes >> 16), (uint8_t)(bytes >> 24)};
    kemu_send(KEMU_T_USB_ACK, f, sizeof f);
}

/* The locks and model outlive every stack and PHY. Static allocation makes
 * first use atomic without allocating memory while holding the spinlock. */
static void ensure(void)
{
    portENTER_CRITICAL(&s_once);
    if (!s_up) {
        s_lock = xSemaphoreCreateMutexStatic(&s_model_storage);
        s_delivery = xSemaphoreCreateMutexStatic(&s_delivery_storage);
        s_stack = xSemaphoreCreateRecursiveMutexStatic(&s_stack_storage);
        s_driver = xSemaphoreCreateMutexStatic(&s_driver_storage);
        kemu_usb_init(&s_m, (kemu_usb_out_t){m_in, m_dev, m_ack}, NULL);
        s_up = true;
    }
    portEXIT_CRITICAL(&s_once);
}

void kemu_usb_stack_lock(void) { ensure(); xSemaphoreTakeRecursive(s_stack, portMAX_DELAY); }
void kemu_usb_stack_unlock(void) { xSemaphoreGiveRecursive(s_stack); }
void kemu_usb_driver_lock(void) { ensure(); xSemaphoreTake(s_driver, portMAX_DELAY); }
void kemu_usb_driver_unlock(void) { xSemaphoreGive(s_driver); }

static void set_stopping(bool stopping)
{
    portENTER_CRITICAL(&s_once);
    s_stopping = stopping;
    portEXIT_CRITICAL(&s_once);
}

void kemu_usb_consumer_set(TaskHandle_t task)
{
    portENTER_CRITICAL(&s_once);
    if (task == NULL || !s_stopping) { s_consumer = task; }
    portEXIT_CRITICAL(&s_once);
}

TaskHandle_t kemu_usb_consumer_get(void)
{
    portENTER_CRITICAL(&s_once);
    TaskHandle_t const task = s_consumer;
    portEXIT_CRITICAL(&s_once);
    return task;
}

bool kemu_usb_in_consumer_task(void)
{
    TaskHandle_t const task = kemu_usb_consumer_get();
    return task != NULL && task == xTaskGetCurrentTaskHandle();
}

bool kemu_usb_in_event_task(void)
{
#ifdef KEMU_USB_TINYUSB
    return s_events != NULL && xTaskGetCurrentTaskHandle() == s_events;
#else
    return false;
#endif
}

/* Exclude the consumer's DCD calls while its owner deletes that task. The
 * virtual IRQ task outlives the consumer and is joined by the delivery
 * barrier, so its DCD callbacks must not wait on the lifecycle mutex. */
static bool lock_dcd(void)
{
    ensure();
    if (kemu_usb_in_event_task()) { return false; }
    kemu_usb_stack_lock();
    return true;
}

#ifdef KEMU_USB_TINYUSB
/* Only this task sends events. A task-context dcd_event_handler can block
 * on TinyUSB's queue; neither the model nor its consumer may be locked then.
 * The delivery lock is also the teardown barrier, acquired before ESP-TinyUSB
 * deletes its consumer task (see kemu_esp_tinyusb.c). */
static void deliver(void *arg)
{
    (void)arg;
    for (;;) {
        ulTaskNotifyTake(pdTRUE, portMAX_DELAY);
        xSemaphoreTake(s_delivery, portMAX_DELAY);
        for (;;) {
            kemu_usb_ev_t ev;
            dcd_event_t t = {0};
            xSemaphoreTake(s_lock, portMAX_DELAY);
            uint32_t const n = s_ints ? kemu_usb_take_events(&s_m, &ev, 1U) : 0U;
            t.rhport = s_rhport;
            xSemaphoreGive(s_lock);
            if (n == 0U) {
                break;
            }
            switch (ev.kind) {
            case KEMU_USB_EV_BUS_RESET: t.event_id = DCD_EVENT_BUS_RESET; t.bus_reset.speed = TUSB_SPEED_FULL; break;
            case KEMU_USB_EV_UNPLUGGED: t.event_id = DCD_EVENT_UNPLUGGED; break;
            case KEMU_USB_EV_SUSPEND: t.event_id = DCD_EVENT_SUSPEND; break;
            case KEMU_USB_EV_RESUME: t.event_id = DCD_EVENT_RESUME; break;
            case KEMU_USB_EV_SETUP:
                t.event_id = DCD_EVENT_SETUP_RECEIVED;
                memcpy(&t.setup_received, ev.setup, sizeof t.setup_received);
                break;
            case KEMU_USB_EV_XFER:
                t.event_id = DCD_EVENT_XFER_COMPLETE;
                t.xfer_complete.ep_addr = ev.ep_addr;
                t.xfer_complete.result = ev.ok ? XFER_RESULT_SUCCESS : XFER_RESULT_FAILED;
                t.xfer_complete.len = ev.len;
                break;
            }
            if (dcd_event_handler != NULL) {
                s_sending_event = &t;
                dcd_event_handler(&t, false);
                s_sending_event = NULL;
            }
        }
        xSemaphoreGive(s_delivery);
    }
}

bool kemu_usb_prepare(void)
{
    /* The stack lifecycle lock serializes creation, including direct users.
     * Reject an independently claimed DCD before TinyUSB allocates anything. */
    xSemaphoreTake(s_lock, portMAX_DELAY);
    bool const available = !s_m.inited;
    xSemaphoreGive(s_lock);
    if (!available) { return false; }
    if (s_events == NULL) {
        return xTaskCreate(deliver, "kemu_usb", CONFIG_KEMU_USB_EVENT_STACK_SIZE, NULL,
                           tskIDLE_PRIORITY + 1U, &s_events) == pdPASS;
    }
    return true;
}

/* Observe the queue used by the real handler, without changing its sends.
 * A stopped direct consumer can leave that send blocked on a full queue.
 * Teardown flushes that retiring queue before waiting for the IRQ barrier;
 * otherwise deleting the consumer before tusb_deinit() could deadlock. */
extern __typeof__(xQueueGenericSend) __real_xQueueGenericSend;
extern __typeof__(xQueueGenericSend) __wrap_xQueueGenericSend;
BaseType_t __wrap_xQueueGenericSend(QueueHandle_t queue, const void *item,
                                  TickType_t wait, BaseType_t position)
{
    if (s_events != NULL && xTaskGetCurrentTaskHandle() == s_events &&
        s_sending_event != NULL && item == s_sending_event) {
        portENTER_CRITICAL(&s_once);
        s_event_queue = queue;
        portEXIT_CRITICAL(&s_once);
    }
    return __real_xQueueGenericSend(queue, item, wait, position);
}

static void wake_events(void)
{
    if (s_events != NULL) {
        xTaskNotifyGive(s_events);
    }
}
#else
static void wake_events(void) { }
#endif

/* Wait for the last send while the consumer can still drain its queue.
 * Holding the barrier also closes the check/send race with dcd_int_disable. */
bool kemu_usb_pause(void)
{
    ensure();
    xSemaphoreTake(s_lock, portMAX_DELAY);
    bool const was_enabled = s_ints;
    s_ints = false;
    ++s_mask_depth;
    xSemaphoreGive(s_lock);
    if (!kemu_usb_in_event_task()) {
        xSemaphoreTake(s_delivery, portMAX_DELAY);
        xSemaphoreGive(s_delivery);
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    s_ints = false;
    --s_mask_depth;
    xSemaphoreGive(s_lock);
    return was_enabled;
}

bool kemu_usb_stop_events(void)
{
    ensure();
    xSemaphoreTake(s_lock, portMAX_DELAY);
    bool const enabled = s_ints;
    s_ints = false;
    set_stopping(true);
    xSemaphoreGive(s_lock);
    while (xSemaphoreTake(s_delivery, 0) != pdTRUE) {
#ifdef KEMU_USB_TINYUSB
        portENTER_CRITICAL(&s_once);
        QueueHandle_t const queue = s_event_queue;
        portEXIT_CRITICAL(&s_once);
        if (queue != NULL) { xQueueReset(queue); }
#endif
        /* The sender must finish before the stack can delete this queue.
         * Yield even if the consumer has already been deleted or suspended. */
        vTaskDelay(1);
    }
    xSemaphoreGive(s_delivery);
    return enabled;
}

void kemu_usb_restore(bool enabled)
{
    xSemaphoreTake(s_lock, portMAX_DELAY);
    s_ints = enabled && s_m.inited;
    set_stopping(!s_m.inited);
    xSemaphoreGive(s_lock);
    wake_events();
}

#define WITH_MODEL(call) do { bool const dcd_locked_ = lock_dcd(); \
    xSemaphoreTake(s_lock, portMAX_DELAY); call; xSemaphoreGive(s_lock); wake_events(); \
    if (dcd_locked_) { kemu_usb_stack_unlock(); } } while (0)

static void on_bus(const uint8_t *p, uint32_t len)
{
    if (len >= 1U) { WITH_MODEL(kemu_usb_bus(&s_m, p[0])); }
}

static void on_setup(const uint8_t *p, uint32_t len)
{
    if (len >= 8U) { WITH_MODEL(kemu_usb_setup(&s_m, p)); }
}

static void on_out(const uint8_t *p, uint32_t len)
{
    if (len >= 1U) { WITH_MODEL((void)kemu_usb_out_data(&s_m, p[0], p + 1, len - 1U)); }
}

void kemu_usb_start(void)
{
    ensure();
    kemu_on(KEMU_T_USB_BUS, on_bus);
    kemu_on(KEMU_T_USB_SETUP, on_setup);
    kemu_on(KEMU_T_USB_OUT, on_out);
}

esp_err_t __wrap_usb_new_phy(const usb_phy_config_t *conf, usb_phy_handle_t *handle)
{
    if (conf == NULL || handle == NULL) { return ESP_ERR_INVALID_ARG; }
    if (kemu_usb_in_event_task()) { return ESP_ERR_INVALID_STATE; }
    if (conf->controller >= USB_PHY_CTRL_MAX || conf->target >= USB_PHY_TARGET_MAX ||
        conf->otg_mode >= USB_OTG_MODE_MAX || conf->otg_speed >= USB_PHY_SPEED_MAX) {
        return ESP_ERR_INVALID_ARG;
    }
    if (conf->controller != USB_PHY_CTRL_OTG || conf->target != USB_PHY_TARGET_INT ||
        conf->otg_mode != USB_OTG_MODE_DEVICE ||
        (conf->otg_speed != USB_PHY_SPEED_UNDEFINED && conf->otg_speed != USB_PHY_SPEED_FULL)) {
        ESP_LOGE("kemu_usb", "Only the internal Full-Speed OTG device PHY is emulated");
        return ESP_ERR_NOT_SUPPORTED;
    }
    esp_err_t ret = ESP_OK;
    kemu_usb_stack_lock();
    xSemaphoreTake(s_lock, portMAX_DELAY);
    if (s_phy != NULL || s_m.inited) {
        ret = ESP_ERR_INVALID_STATE;
    } else {
        s_phy = calloc(1U, sizeof(usb_phy_config_t));
        if (s_phy == NULL) { ret = ESP_ERR_NO_MEM; }
        else { *handle = (usb_phy_handle_t)s_phy; }
    }
    xSemaphoreGive(s_lock);
    kemu_usb_stack_unlock();
    return ret;
}

esp_err_t __wrap_usb_del_phy(usb_phy_handle_t handle)
{
    if (kemu_usb_in_event_task()) { return ESP_ERR_INVALID_STATE; }
    ensure();
    kemu_usb_stack_lock();
    /* Invalid/double deletion must not disconnect another owner's device. */
    xSemaphoreTake(s_lock, portMAX_DELAY);
    bool const valid = handle != NULL && (void *)handle == s_phy;
    xSemaphoreGive(s_lock);
    if (!valid) { kemu_usb_stack_unlock(); return ESP_ERR_INVALID_ARG; }
    (void)kemu_usb_stop_events();
    xSemaphoreTake(s_lock, portMAX_DELAY);
    kemu_usb_dcd_deinit(&s_m);
    free(s_phy);
    s_phy = NULL;
    xSemaphoreGive(s_lock);
    kemu_usb_stack_unlock();
    return ESP_OK;
}

#ifdef KEMU_USB_TINYUSB
/* P4's DCD convention is port 0 FS, port 1 HS (common/tusb_mcu.h).
 * The build supplies KEMU_USB_FS_RHPORT from the target/ESP-TinyUSB API. */
static bool active(uint8_t rhport) { return rhport == s_rhport && s_m.inited; }

bool __wrap_dcd_init(uint8_t rhport, const tusb_rhport_init_t *rh_init)
{
    if (kemu_usb_in_event_task()) { return false; }
    if (rhport != KEMU_USB_FS_RHPORT || rh_init == NULL || rh_init->role != TUSB_ROLE_DEVICE ||
        (rh_init->speed != TUSB_SPEED_FULL && rh_init->speed != TUSB_SPEED_AUTO)) {
        ESP_LOGE("kemu_usb", "Only the Full-Speed device controller is emulated");
        return false;
    }
    bool ok = false;
    kemu_usb_stack_lock();
    WITH_MODEL({
        ok = kemu_usb_dcd_init(&s_m);
        if (ok) {
            s_rhport = rhport;
            s_event_queue = NULL;
            s_ints = false;
            set_stopping(false);
            kemu_usb_dcd_connect(&s_m);
        }
    });
    kemu_usb_stack_unlock();
    return ok;
}

bool __wrap_dcd_deinit(uint8_t rhport)
{
    if (rhport != KEMU_USB_FS_RHPORT || kemu_usb_in_event_task()) { return false; }
    kemu_usb_stack_lock();
    (void)kemu_usb_stop_events();
    WITH_MODEL({ kemu_usb_dcd_deinit(&s_m); s_event_queue = NULL; kemu_usb_consumer_set(NULL); });
    kemu_usb_stack_unlock();
    return true;
}

/* usbd.c defines this weakly and calls it in that same object. --wrap alone
 * does not replace that reference; a strong DCD definition does. */
bool dcd_deinit(uint8_t rhport) { return __wrap_dcd_deinit(rhport); }

void __wrap_dcd_int_handler(uint8_t rhport) { (void)rhport; }
void __wrap_dcd_int_enable(uint8_t rhport) { WITH_MODEL(if (active(rhport) && !s_stopping && s_mask_depth == 0U) { s_ints = true; }); }
void __wrap_dcd_int_disable(uint8_t rhport)
{
    bool const locked = lock_dcd();
    if (rhport == KEMU_USB_FS_RHPORT) { (void)kemu_usb_pause(); }
    if (locked) { kemu_usb_stack_unlock(); }
}
void __wrap_dcd_remote_wakeup(uint8_t rhport) { (void)rhport; }
void __wrap_dcd_sof_enable(uint8_t rhport, bool en) { (void)rhport; (void)en; }
void __wrap_dcd_edpt0_status_complete(uint8_t rhport, const tusb_control_request_t *request)
{ (void)rhport; (void)request; }

void __wrap_dcd_set_address(uint8_t rhport, uint8_t addr)
{ WITH_MODEL(if (active(rhport)) { kemu_usb_dcd_set_address(&s_m, addr); }); }
void __wrap_dcd_connect(uint8_t rhport)
{ WITH_MODEL(if (active(rhport)) { kemu_usb_dcd_connect(&s_m); }); }
void __wrap_dcd_disconnect(uint8_t rhport)
{ WITH_MODEL(if (active(rhport)) { kemu_usb_dcd_disconnect(&s_m); }); }
/* These also have weak, same-object defaults in usbd.c. */
void dcd_connect(uint8_t rhport) { __wrap_dcd_connect(rhport); }
void dcd_disconnect(uint8_t rhport) { __wrap_dcd_disconnect(rhport); }

bool __wrap_dcd_edpt_open(uint8_t rhport, const tusb_desc_endpoint_t *d)
{
    bool ok = false;
    if (d == NULL) { return false; }
    WITH_MODEL(if (active(rhport)) { ok = kemu_usb_dcd_edpt_open(&s_m, d->bEndpointAddress,
        tu_le16toh(d->wMaxPacketSize) & 0x7FFU, d->bmAttributes.xfer); });
    return ok;
}

bool __wrap_dcd_edpt_iso_alloc(uint8_t rhport, uint8_t ep_addr, uint16_t size)
{
    bool ok = false;
    WITH_MODEL(if (active(rhport)) { ok = kemu_usb_dcd_edpt_open(&s_m, ep_addr, size, TUSB_XFER_ISOCHRONOUS); });
    return ok;
}
bool __wrap_dcd_edpt_iso_activate(uint8_t rhport, const tusb_desc_endpoint_t *d)
{ return __wrap_dcd_edpt_open(rhport, d); }
void __wrap_dcd_edpt_close(uint8_t rhport, uint8_t ep)
{ WITH_MODEL(if (active(rhport)) { kemu_usb_dcd_edpt_close(&s_m, ep); }); }
void __wrap_dcd_edpt_close_all(uint8_t rhport)
{ WITH_MODEL(if (active(rhport)) { kemu_usb_dcd_edpt_close_all(&s_m); }); }

bool __wrap_dcd_edpt_xfer(uint8_t rhport, uint8_t ep, uint8_t *buf, uint16_t len
#if TUSB_VERSION_NUMBER >= 2100
    , bool is_isr
#endif
)
{
#if TUSB_VERSION_NUMBER >= 2100
    if (is_isr) { return false; } /* this virtual controller uses task-context I/O */
#endif
    bool ok = false;
    WITH_MODEL(if (active(rhport)) { ok = kemu_usb_dcd_edpt_xfer(&s_m, ep, buf, len); });
    return ok;
}
bool __wrap_dcd_edpt_xfer_fifo(uint8_t rhport, uint8_t ep, tu_fifo_t *ff, uint16_t len
#if TUSB_VERSION_NUMBER >= 2100
    , bool is_isr
#endif
)
{
    (void)rhport; (void)ep; (void)ff; (void)len;
#if TUSB_VERSION_NUMBER >= 2100
    (void)is_isr;
#endif
    return false;
}
void __wrap_dcd_edpt_stall(uint8_t rhport, uint8_t ep)
{ WITH_MODEL(if (active(rhport)) { kemu_usb_dcd_edpt_stall(&s_m, ep); }); }
void __wrap_dcd_edpt_clear_stall(uint8_t rhport, uint8_t ep)
{ WITH_MODEL(if (active(rhport)) { kemu_usb_dcd_edpt_clear_stall(&s_m, ep); }); }
#endif
