/* Keep unsupported controllers out before TinyUSB allocates its queue, and
 * quiesce the virtual interrupt source before the real stack deletes it. */
#include "esp_log.h"
#include "tusb.h"
#include "kemu_usb_internal.h"

extern bool __real_tusb_rhport_init(uint8_t rhport, const tusb_rhport_init_t *init);
extern bool __real_tud_rhport_init(uint8_t rhport, const tusb_rhport_init_t *init);
extern bool __real_tusb_deinit(uint8_t rhport);
extern bool __real_tud_deinit(uint8_t rhport);
extern __typeof__(tud_task_ext) __real_tud_task_ext;

void __wrap_tud_task_ext(uint32_t timeout_ms, bool in_isr)
{
    if (!in_isr && tud_inited()) { kemu_usb_consumer_set(xTaskGetCurrentTaskHandle()); }
    __real_tud_task_ext(timeout_ms, in_isr);
}

static bool supported(uint8_t rhport, const tusb_rhport_init_t *init)
{
    if (init == NULL || rhport != KEMU_USB_FS_RHPORT || init->role != TUSB_ROLE_DEVICE ||
        (init->speed != TUSB_SPEED_FULL && init->speed != TUSB_SPEED_AUTO)) {
        ESP_LOGE("kemu_usb", "Only the Full-Speed device root port is emulated");
        return false;
    }
    return true;
}

bool __wrap_tud_rhport_init(uint8_t rhport, const tusb_rhport_init_t *init)
{
    if (kemu_usb_in_event_task() || !supported(rhport, init)) { return false; }
    kemu_usb_stack_lock();
    bool const ok = (tud_inited() || kemu_usb_prepare()) && __real_tud_rhport_init(rhport, init);
    kemu_usb_stack_unlock();
    return ok;
}

bool __wrap_tusb_rhport_init(uint8_t rhport, const tusb_rhport_init_t *init)
{
    /* tusb_init() without arguments uses the application's compile-time port. */
#ifdef TUD_OPT_RHPORT
    const tusb_rhport_init_t legacy = {
        .role = TUSB_ROLE_DEVICE,
        .speed = TUD_OPT_HIGH_SPEED ? TUSB_SPEED_HIGH : TUSB_SPEED_FULL,
    };
    if (init == NULL) { rhport = TUD_OPT_RHPORT; init = &legacy; }
#endif
    if (kemu_usb_in_event_task() || !supported(rhport, init)) { return false; }
    kemu_usb_stack_lock();
    bool const ok = (tud_inited() || kemu_usb_prepare()) && __real_tusb_rhport_init(rhport, init);
    kemu_usb_stack_unlock();
    return ok;
}

bool __wrap_tud_deinit(uint8_t rhport)
{
    if (kemu_usb_in_event_task() || kemu_usb_in_consumer_task() || rhport != KEMU_USB_FS_RHPORT) { return false; }
    kemu_usb_stack_lock();
    bool const enabled = kemu_usb_stop_events();
    bool const ok = __real_tud_deinit(rhport);
    if (!ok) { kemu_usb_restore(enabled); }
    kemu_usb_stack_unlock();
    return ok;
}

bool __wrap_tusb_deinit(uint8_t rhport)
{
    if (kemu_usb_in_event_task() || kemu_usb_in_consumer_task() || rhport != KEMU_USB_FS_RHPORT) { return false; }
    kemu_usb_stack_lock();
    bool const enabled = kemu_usb_stop_events();
    bool const ok = __real_tusb_deinit(rhport);
    if (!ok) { kemu_usb_restore(enabled); }
    kemu_usb_stack_unlock();
    return ok;
}
