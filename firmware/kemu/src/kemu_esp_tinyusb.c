/* This archive member is extracted only for callers of ESP-TinyUSB's API.
 * Direct TinyUSB applications must not pull in ESP-TinyUSB's descriptors or
 * callbacks. All validation, task/descriptor ownership and return values stay
 * in the real driver; only unsupported hardware and teardown ordering differ. */
#include "esp_log.h"
#include "tinyusb.h"
#include "kemu_usb_internal.h"

extern esp_err_t __real_tinyusb_driver_install(const tinyusb_config_t *config);
extern esp_err_t __real_tinyusb_driver_uninstall(void);
static bool s_installed;

esp_err_t __wrap_tinyusb_driver_install(const tinyusb_config_t *config)
{
    if (kemu_usb_in_event_task()) { return ESP_ERR_INVALID_STATE; }
#if SOC_USB_OTG_PERIPH_NUM > 1
    if (config != NULL && config->port == TINYUSB_PORT_HIGH_SPEED_0) {
        ESP_LOGE("kemu_usb", "High-Speed USB is not emulated; select TINYUSB_PORT_FULL_SPEED_0");
        return ESP_ERR_NOT_SUPPORTED;
    }
#endif
    kemu_usb_driver_lock();
    esp_err_t const ret = __real_tinyusb_driver_install(config);
    if (ret == ESP_OK) { s_installed = true; }
    kemu_usb_driver_unlock();
    return ret;
}

esp_err_t __wrap_tinyusb_driver_uninstall(void)
{
    if (kemu_usb_in_event_task() || kemu_usb_in_consumer_task()) { return ESP_ERR_INVALID_STATE; }
    kemu_usb_driver_lock();
    /* tinyusb_task_stop() deletes its task before tusb_deinit(). Drain the
     * in-flight send while that task still exists, then mask the source. */
    kemu_usb_stack_lock();
    bool const enabled = s_installed ? kemu_usb_stop_events() : false;
    esp_err_t const ret = __real_tinyusb_driver_uninstall();
    if (ret == ESP_OK) { s_installed = false; }
    else if (s_installed) { kemu_usb_restore(enabled); }
    kemu_usb_stack_unlock();
    kemu_usb_driver_unlock();
    return ret;
}
