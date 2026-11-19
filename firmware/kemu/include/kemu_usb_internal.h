/* USB hardware/lifecycle synchronization, private to kemu. */
#ifndef KEMU_USB_INTERNAL_H_
#define KEMU_USB_INTERNAL_H_
#include <stdbool.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#ifdef KEMU_USB_ESP_TINYUSB
#include "tinyusb.h"
#define KEMU_USB_FS_RHPORT TINYUSB_PORT_FULL_SPEED_0
#else
/* TinyUSB common/tusb_mcu.h: ESP32-P4 port 0 is FS, port 1 is HS. */
#define KEMU_USB_FS_RHPORT 0
#endif
void kemu_usb_stack_lock(void);
void kemu_usb_stack_unlock(void);
void kemu_usb_driver_lock(void);
void kemu_usb_driver_unlock(void);
bool kemu_usb_prepare(void);
bool kemu_usb_in_event_task(void);
void kemu_usb_consumer_set(TaskHandle_t task);
TaskHandle_t kemu_usb_consumer_get(void);
bool kemu_usb_in_consumer_task(void);
bool kemu_usb_pause(void);
bool kemu_usb_stop_events(void);
void kemu_usb_restore(bool enabled);
#endif
