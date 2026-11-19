/* Forward task deletion while excluding DCD critical sections. Only the
 * observed TinyUSB consumer also needs an IRQ-delivery barrier. This member
 * must not extract the stack for applications deleting unrelated tasks. */
#include "esp_log.h"
#include "kemu_usb_internal.h"

extern __typeof__(vTaskDelete) __real_vTaskDelete;
extern __typeof__(vTaskDelete) __wrap_vTaskDelete;

void __wrap_vTaskDelete(TaskHandle_t task)
{
    if (kemu_usb_in_event_task()) {
        ESP_LOGE("kemu_usb", "Delete tasks outside the virtual interrupt callback");
        return;
    }
    TaskHandle_t const current = xTaskGetCurrentTaskHandle();
    TaskHandle_t const target = task != NULL ? task : current;
    if (target == current && target != kemu_usb_consumer_get()) {
        __real_vTaskDelete(task);
        return;
    }
    /* Include foreign deletion before the consumer's first tud_task_ext():
     * it can start a DCD call between task creation and this deletion. */
    kemu_usb_stack_lock();
    bool const consumer = target == kemu_usb_consumer_get();
    if (consumer) {
        (void)kemu_usb_stop_events();
        kemu_usb_consumer_set(NULL);
    }
    if (target == current) {
        /* A self-delete cannot release locks after the real call. */
        kemu_usb_stack_unlock();
        __real_vTaskDelete(task);
        return;
    }
    __real_vTaskDelete(task);
    if (target == kemu_usb_consumer_get()) {
        /* Registration may have raced the initial consumer check. */
        (void)kemu_usb_stop_events();
        kemu_usb_consumer_set(NULL);
    }
    kemu_usb_stack_unlock();
}
