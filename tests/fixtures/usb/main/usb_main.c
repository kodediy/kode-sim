/* A boot keyboard exercised through either the real ESP-TinyUSB driver or
 * an application's own TinyUSB lifecycle. No board services are started. */
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <ctype.h>
#include <errno.h>
#include "driver/usb_serial_jtag.h"
#include "esp_err.h"
#include "esp_idf_version.h"
#include "esp_heap_caps.h"
#include "esp_private/usb_phy.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/semphr.h"
#include "tinyusb.h"
#include "tinyusb_default_config.h"
#include "device/dcd.h"
#ifdef CONFIG_USB_TEST_MSC
#include "esp_partition.h"
#include "tinyusb_msc.h"
#endif

#ifdef TUP_DCD_EDPT_ISO_ALLOC
/* The virtual DCD also implements per-endpoint close; P4's hardware DCD
 * advertises only ISO allocation and leaves this declaration out. */
extern void dcd_edpt_close(uint8_t rhport, uint8_t ep_addr);
#endif

/* IDs are fixture data and are shared by the descriptor and sender. The
 * host must discover them from the descriptors, including these non-boot IDs. */
enum { TEST_REPORT_KEYBOARD = 43, TEST_REPORT_MOUSE = 207 };
#if defined(CONFIG_USB_TEST_SEPARATE)
enum { TEST_ITF_KEYBOARD, TEST_ITF_MOUSE, TEST_ITF_COUNT };
enum { TEST_INSTANCE_KEYBOARD, TEST_INSTANCE_MOUSE };
static const uint8_t keyboard_desc[] = { TUD_HID_REPORT_DESC_KEYBOARD() };
static const uint8_t mouse_desc[] = { TUD_HID_REPORT_DESC_MOUSE() };
static const uint8_t config_desc[] = {
    TUD_CONFIG_DESCRIPTOR(1, TEST_ITF_COUNT, 0, TUD_CONFIG_DESC_LEN + TEST_ITF_COUNT * TUD_HID_DESC_LEN,
                          TUSB_DESC_CONFIG_ATT_REMOTE_WAKEUP, 100),
    TUD_HID_DESCRIPTOR(TEST_ITF_KEYBOARD, 0, HID_ITF_PROTOCOL_KEYBOARD, sizeof keyboard_desc,
                       0x81, CFG_TUD_HID_EP_BUFSIZE, 10),
    TUD_HID_DESCRIPTOR(TEST_ITF_MOUSE, 0, HID_ITF_PROTOCOL_MOUSE, sizeof mouse_desc,
                       0x82, CFG_TUD_HID_EP_BUFSIZE, 10),
};
#else
enum { TEST_INSTANCE_KEYBOARD, TEST_INSTANCE_MOUSE = TEST_INSTANCE_KEYBOARD };
static const uint8_t report_desc[] = {
#if defined(CONFIG_USB_TEST_SHARED) || defined(CONFIG_USB_TEST_MSC)
    TUD_HID_REPORT_DESC_KEYBOARD(HID_REPORT_ID(TEST_REPORT_KEYBOARD)),
    TUD_HID_REPORT_DESC_MOUSE(HID_REPORT_ID(TEST_REPORT_MOUSE)),
#elif defined(CONFIG_USB_TEST_MOUSE)
    TUD_HID_REPORT_DESC_MOUSE(),
#else
    TUD_HID_REPORT_DESC_KEYBOARD(),
#endif
};
#ifdef CONFIG_USB_TEST_MSC
enum { TEST_ITF_COUNT = 2, TEST_CONFIG_LENGTH = TUD_CONFIG_DESC_LEN + TUD_HID_DESC_LEN + TUD_MSC_DESC_LEN };
#else
enum { TEST_ITF_COUNT = 1, TEST_CONFIG_LENGTH = TUD_CONFIG_DESC_LEN + TUD_HID_DESC_LEN };
#endif
static const uint8_t config_desc[] = {
    TUD_CONFIG_DESCRIPTOR(1, TEST_ITF_COUNT, 0, TEST_CONFIG_LENGTH,
                          TUSB_DESC_CONFIG_ATT_REMOTE_WAKEUP, 100),
#if defined(CONFIG_USB_TEST_SHARED) || defined(CONFIG_USB_TEST_MSC)
    TUD_HID_DESCRIPTOR(0, 0, HID_ITF_PROTOCOL_NONE, sizeof report_desc,
                       0x81, CFG_TUD_HID_EP_BUFSIZE, 10),
#elif defined(CONFIG_USB_TEST_MOUSE)
    TUD_HID_DESCRIPTOR(0, 0, HID_ITF_PROTOCOL_MOUSE, sizeof report_desc,
                       0x81, CFG_TUD_HID_EP_BUFSIZE, 10),
#else
    TUD_HID_DESCRIPTOR(0, 0, HID_ITF_PROTOCOL_KEYBOARD, sizeof report_desc,
                       0x81, sizeof(hid_keyboard_report_t), 10),
#endif
#ifdef CONFIG_USB_TEST_MSC
    TUD_MSC_DESCRIPTOR(1, 0, 0x02, 0x82, 64),
#endif
};
#endif
static const tusb_desc_device_t device_desc = {
    .bLength = sizeof(tusb_desc_device_t), .bDescriptorType = TUSB_DESC_DEVICE,
    .bcdUSB = 0x0200, .bMaxPacketSize0 = CFG_TUD_ENDPOINT0_SIZE,
    .idVendor = TINYUSB_ESPRESSIF_VID, .idProduct = 0x4004, .bcdDevice = 0x0100,
    .iManufacturer = 1, .iProduct = 2, .bNumConfigurations = 1,
};
static const char *strings[] = { (const char[]){0x09, 0x04}, "kode-sim", "ESP-TinyUSB lifecycle" };
static unsigned mounts, unmounts;

#ifdef CONFIG_USB_TEST_MSC
static void storage_init(void)
{
    const esp_partition_t *partition = esp_partition_find_first(ESP_PARTITION_TYPE_DATA, ESP_PARTITION_SUBTYPE_DATA_FAT, NULL);
    configASSERT(partition != NULL);
    wl_handle_t wl;
    ESP_ERROR_CHECK(wl_mount(partition, &wl));
    printf("USB storage sector=%zu\n", wl_sector_size(wl));
    const tinyusb_msc_storage_config_t config = {
        .medium.wl_handle = wl,
        .fat_fs = {
            .base_path = "/usb-test",
            .config = { .format_if_mount_failed = true, .max_files = 1 },
        },
        .mount_point = TINYUSB_MSC_STORAGE_MOUNT_APP,
    };
    ESP_ERROR_CHECK(tinyusb_msc_new_storage_spiflash(&config, NULL));
}
#endif

uint8_t const *tud_hid_descriptor_report_cb(uint8_t instance)
{
#if defined(CONFIG_USB_TEST_SEPARATE)
    if (instance == TEST_INSTANCE_KEYBOARD) { return keyboard_desc; }
    if (instance == TEST_INSTANCE_MOUSE) { return mouse_desc; }
    return NULL;
#else
    return instance == TEST_INSTANCE_KEYBOARD ? report_desc : NULL;
#endif
}

void tud_hid_report_complete_cb(uint8_t instance, uint8_t const *report, uint16_t len)
{
    (void)report;
    printf("USB report complete instance=%u length=%u\n", (unsigned)instance, (unsigned)len);
}
uint16_t tud_hid_get_report_cb(uint8_t instance, uint8_t report_id, hid_report_type_t type,
                              uint8_t *buffer, uint16_t size)
{ (void)instance; (void)report_id; (void)type; (void)buffer; (void)size; return 0; }
void tud_hid_set_report_cb(uint8_t instance, uint8_t report_id, hid_report_type_t type,
                           const uint8_t *buffer, uint16_t size)
{ (void)instance; (void)report_id; (void)type; (void)buffer; (void)size; }

/* Exercise DCD calls from the virtual interrupt callback, including when an
 * uninstall overlaps a transfer completion. These calls must not wait on
 * the dispatcher's own delivery barrier or the consumer's lifecycle lock. */
void tud_event_hook_cb(uint8_t rhport, uint32_t event_id, bool in_isr)
{
    if (event_id == DCD_EVENT_XFER_COMPLETE) {
        configASSERT(!in_isr);
        dcd_int_disable(rhport);
        dcd_int_enable(rhport);
    }
}

static void mounted(uint8_t rhport)
{
    printf("USB attached %u rhport=%u speed=%s configuration=%u\n",
           ++mounts, rhport, tud_speed_get() == TUSB_SPEED_FULL ? "full" : "other", tud_mounted() ? 1U : 0U);
}
static void unmounted(uint8_t rhport)
{ printf("USB detached %u rhport=%u\n", ++unmounts, rhport); }

#ifdef CONFIG_USB_TEST_DIRECT
static usb_phy_handle_t phy;
static TaskHandle_t task;
uint8_t const *tud_descriptor_device_cb(void) { return (const uint8_t *)&device_desc; }
uint8_t const *tud_descriptor_configuration_cb(uint8_t index) { (void)index; return config_desc; }
uint16_t const *tud_descriptor_string_cb(uint8_t index, uint16_t langid)
{
    (void)langid;
    static uint16_t desc[32];
    if (index >= sizeof strings / sizeof strings[0]) { return NULL; }
    size_t n = index == 0 ? 1U : strlen(strings[index]);
    if (n > (sizeof desc / sizeof desc[0]) - 1U) { return NULL; }
    if (index == 0) { memcpy(&desc[1], strings[0], 2U); }
    else { for (size_t i = 0; i < n; ++i) { desc[i + 1U] = (uint8_t)strings[index][i]; } }
    desc[0] = (uint16_t)((TUSB_DESC_STRING << 8) | (2U * (n + 1U)));
    return desc;
}
void tud_mount_cb(void) { mounted(TINYUSB_PORT_FULL_SPEED_0); }
void tud_umount_cb(void) { unmounted(TINYUSB_PORT_FULL_SPEED_0); }
static void device_task(void *arg) { (void)arg; for (;;) { tud_task(); } }
static esp_err_t install(void)
{
    if (task != NULL) { return ESP_ERR_INVALID_STATE; }
    const usb_phy_config_t cfg = {
        .controller = USB_PHY_CTRL_OTG, .target = USB_PHY_TARGET_INT,
        .otg_mode = USB_OTG_MODE_DEVICE, .otg_speed = USB_PHY_SPEED_FULL,
    };
    esp_err_t ret = usb_new_phy(&cfg, &phy);
    if (ret != ESP_OK) { return ret; }
    const tusb_rhport_init_t init = { .role = TUSB_ROLE_DEVICE, .speed = TUSB_SPEED_FULL };
    if (!tusb_init(TINYUSB_PORT_FULL_SPEED_0, &init)) {
        usb_del_phy(phy); phy = NULL; return ESP_FAIL;
    }
    if (xTaskCreate(device_task, "direct_usb", TINYUSB_DEFAULT_TASK_SIZE, NULL,
                    TINYUSB_DEFAULT_TASK_PRIO, &task) != pdPASS) {
        tusb_deinit(TINYUSB_PORT_FULL_SPEED_0);
        usb_del_phy(phy); phy = NULL; return ESP_ERR_NO_MEM;
    }
    return ESP_OK;
}
static esp_err_t uninstall(void)
{
    if (task == NULL) { return ESP_ERR_INVALID_STATE; }
    vTaskDelete(task); task = NULL;
    if (!tusb_deinit(TINYUSB_PORT_FULL_SPEED_0)) { return ESP_FAIL; }
    esp_err_t ret = usb_del_phy(phy); phy = NULL; return ret;
}
#else
static void usb_event(tinyusb_event_t *event, void *arg)
{
    (void)arg;
    if (event->id == TINYUSB_EVENT_ATTACHED) { mounted(event->rhport); }
    if (event->id == TINYUSB_EVENT_DETACHED) { unmounted(event->rhport); }
}
static tinyusb_config_t configuration(void)
{
    tinyusb_config_t cfg = TINYUSB_DEFAULT_CONFIG(usb_event);
    cfg.port = TINYUSB_PORT_FULL_SPEED_0;
    cfg.descriptor.device = &device_desc;
    cfg.descriptor.full_speed_config = config_desc;
    cfg.descriptor.string = strings;
    cfg.descriptor.string_count = sizeof strings / sizeof strings[0];
    return cfg;
}
static esp_err_t install(void) { tinyusb_config_t cfg = configuration(); return tinyusb_driver_install(&cfg); }
static esp_err_t uninstall(void) { return tinyusb_driver_uninstall(); }

static SemaphoreHandle_t race_done;
static void race_install(void *arg)
{
    esp_err_t ret = install();
    printf("USB race %u %s\n", (unsigned)(uintptr_t)arg, esp_err_to_name(ret));
    xSemaphoreGive(race_done);
    vTaskDelete(NULL);
}
#endif

static bool byte_argument(const char **text, unsigned base, uint8_t *value)
{
    while (isspace((unsigned char)**text)) { ++*text; }
    if (!isxdigit((unsigned char)**text)) { return false; }
    char *end;
    errno = 0;
    unsigned long const number = strtoul(*text, &end, base);
    if (end == *text || errno != 0 || number > UINT8_MAX || (*end != '\0' && !isspace((unsigned char)*end))) {
        return false;
    }
    *text = end;
    while (isspace((unsigned char)**text)) { ++*text; }
    *value = (uint8_t)number;
    return true;
}

static const tusb_desc_endpoint_t *input_endpoint(uint8_t instance)
{
    uint8_t hid_instance = 0;
    bool selected = false;
    for (const uint8_t *p = config_desc; p < config_desc + sizeof config_desc; p += tu_desc_len(p)) {
        if (tu_desc_type(p) == TUSB_DESC_INTERFACE) {
            const tusb_desc_interface_t *itf = (const tusb_desc_interface_t *)p;
            selected = itf->bInterfaceClass == TUSB_CLASS_HID && hid_instance++ == instance;
        } else if (selected && tu_desc_type(p) == TUSB_DESC_ENDPOINT) {
            const tusb_desc_endpoint_t *ep = (const tusb_desc_endpoint_t *)p;
            if (tu_edpt_dir(ep->bEndpointAddress) == TUSB_DIR_IN && ep->bmAttributes.xfer == TUSB_XFER_INTERRUPT) {
                return ep;
            }
        }
    }
    return NULL;
}

static void command(const char *line)
{
    esp_err_t ret = ESP_ERR_INVALID_ARG;
    if (strcmp(line, "install") == 0) { ret = install(); }
    else if (strcmp(line, "uninstall") == 0) { ret = uninstall(); }
    else if (strcmp(line, "disconnect") == 0) { ret = tud_disconnect() ? ESP_OK : ESP_FAIL; }
    else if (strcmp(line, "connect") == 0) { ret = tud_connect() ? ESP_OK : ESP_FAIL; }
    else if (strcmp(line, "mask") == 0) { dcd_int_disable(TINYUSB_PORT_FULL_SPEED_0); ret = ESP_OK; }
    else if (strcmp(line, "unmask") == 0) { dcd_int_enable(TINYUSB_PORT_FULL_SPEED_0); ret = ESP_OK; }
#ifdef CONFIG_USB_TEST_DIRECT
    else if (strcmp(line, "pause_consumer") == 0) { vTaskSuspend(task); ret = ESP_OK; }
#endif
    else if (strcmp(line, "yield") == 0) { vTaskDelay(1); ret = ESP_OK; }
    else if (strcmp(line, "bad_phy") == 0) { ret = usb_del_phy((usb_phy_handle_t)&mounts); }
    else if (strcmp(line, "resources") == 0) {
        vTaskDelay(1);
        ret = heap_caps_check_integrity_all(true) ? ESP_OK : ESP_FAIL;
        printf("USB resources heap=%zu tasks=%u\n", heap_caps_get_free_size(MALLOC_CAP_8BIT),
               (unsigned)uxTaskGetNumberOfTasks());
    }
    else if (strcmp(line, "report") == 0) {
#if !defined(CONFIG_USB_TEST_MOUSE)
        uint8_t keys[6] = { HID_KEY_A };
        ret = tud_hid_n_keyboard_report(TEST_INSTANCE_KEYBOARD,
#if defined(CONFIG_USB_TEST_SHARED) || defined(CONFIG_USB_TEST_MSC)
                                       TEST_REPORT_KEYBOARD,
#else
                                       0,
#endif
                                       0, keys) ? ESP_OK : ESP_FAIL;
#else
        ret = ESP_ERR_NOT_SUPPORTED;
#endif
    }
#ifndef CONFIG_USB_TEST_DIRECT
    else if (strcmp(line, "invalid") == 0) { ret = tinyusb_driver_install(NULL); }
    else if (strcmp(line, "high_speed") == 0 || strcmp(line, "high_speed_skip_phy") == 0) {
        tinyusb_config_t cfg = configuration(); cfg.port = TINYUSB_PORT_HIGH_SPEED_0;
        cfg.phy.skip_setup = strcmp(line, "high_speed_skip_phy") == 0;
        ret = tinyusb_driver_install(&cfg);
    } else if (strcmp(line, "bad_task") == 0) {
        tinyusb_config_t cfg = configuration(); cfg.task.size = 0; ret = tinyusb_driver_install(&cfg);
    } else if (strcmp(line, "bad_descriptors") == 0) {
        tinyusb_config_t cfg = configuration(); cfg.descriptor.full_speed_config = NULL;
        ret = tinyusb_driver_install(&cfg);
    } else if (strcmp(line, "skip_phy") == 0) {
        tinyusb_config_t cfg = configuration(); cfg.phy.skip_setup = true; ret = tinyusb_driver_install(&cfg);
    } else if (strcmp(line, "race") == 0) {
        for (uintptr_t i = 0; i < 2U; ++i) {
            configASSERT(xTaskCreate(race_install, "usb_race", TINYUSB_DEFAULT_TASK_SIZE,
                         (void *)i, TINYUSB_DEFAULT_TASK_PRIO, NULL) == pdPASS);
        }
        xSemaphoreTake(race_done, portMAX_DELAY); xSemaphoreTake(race_done, portMAX_DELAY);
        ret = ESP_OK;
    }
#endif
    else if (strncmp(line, "key ", 4U) == 0) {
#if !defined(CONFIG_USB_TEST_MOUSE)
        hid_keyboard_report_t report = {0};
        const char *args = line + 4;
        bool valid = byte_argument(&args, 10, &report.modifier);
        size_t count = 0;
        while (valid && *args != '\0') {
            valid = count < sizeof report.keycode && byte_argument(&args, 10, &report.keycode[count]);
            ++count;
        }
        if (valid && count != 0U) {
            ret = tud_hid_n_keyboard_report(TEST_INSTANCE_KEYBOARD,
#if defined(CONFIG_USB_TEST_SHARED) || defined(CONFIG_USB_TEST_MSC)
                                           TEST_REPORT_KEYBOARD,
#else
                                           0,
#endif
                                           report.modifier, report.keycode) ? ESP_OK : ESP_FAIL;
        }
#else
        ret = ESP_ERR_NOT_SUPPORTED;
#endif
    } else if (strncmp(line, "mouse ", 6U) == 0) {
#if defined(CONFIG_USB_TEST_MOUSE) || defined(CONFIG_USB_TEST_SEPARATE) || defined(CONFIG_USB_TEST_SHARED) || defined(CONFIG_USB_TEST_MSC)
        unsigned buttons; int x, y, wheel; char extra;
        if (sscanf(line + 6, "%u %d %d %d %c", &buttons, &x, &y, &wheel, &extra) == 4 && buttons <= UINT8_MAX
            && x >= INT8_MIN && x <= INT8_MAX && y >= INT8_MIN && y <= INT8_MAX && wheel >= INT8_MIN && wheel <= INT8_MAX) {
            ret = tud_hid_n_mouse_report(TEST_INSTANCE_MOUSE,
#if defined(CONFIG_USB_TEST_SHARED) || defined(CONFIG_USB_TEST_MSC)
                                        TEST_REPORT_MOUSE,
#else
                                        0,
#endif
                                        (uint8_t)buttons, (int8_t)x, (int8_t)y, (int8_t)wheel, 0) ? ESP_OK : ESP_FAIL;
        }
#else
        ret = ESP_ERR_NOT_SUPPORTED;
#endif
    } else if (strncmp(line, "raw ", 4U) == 0) {
        const char *args = line + 4;
        uint8_t instance, report_id, data[CFG_TUD_HID_EP_BUFSIZE];
        bool valid = byte_argument(&args, 10, &instance) && byte_argument(&args, 10, &report_id);
        size_t count = 0;
        while (valid && *args != '\0') {
            valid = count < sizeof data && byte_argument(&args, 16, &data[count]);
            ++count;
        }
        if (valid) { ret = tud_hid_n_report(instance, report_id, data, count) ? ESP_OK : ESP_FAIL; }
    } else if (strncmp(line, "endpoint ", 9U) == 0) {
        const char *args = line + 9;
        uint8_t instance, open;
        if (byte_argument(&args, 10, &instance) && byte_argument(&args, 10, &open) && *args == '\0' && open <= 1U) {
            const tusb_desc_endpoint_t *ep = input_endpoint(instance);
            if (ep != NULL && tud_mounted()) {
                if (open) { ret = dcd_edpt_open(TINYUSB_PORT_FULL_SPEED_0, ep) ? ESP_OK : ESP_FAIL; }
                else { dcd_edpt_close(TINYUSB_PORT_FULL_SPEED_0, ep->bEndpointAddress); ret = ESP_OK; }
            }
        }
    }
    printf("USB result %s %s\n", line, esp_err_to_name(ret));
    fflush(stdout);
}

void app_main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
#ifdef CONFIG_USB_TEST_MSC
    storage_init();
#endif
    printf("USB versions idf=%s tinyusb=%u.%u.%u\n", esp_get_idf_version(),
           TUSB_VERSION_MAJOR, TUSB_VERSION_MINOR, TUSB_VERSION_REVISION);
#ifndef CONFIG_USB_TEST_DIRECT
    race_done = xSemaphoreCreateCounting(2U, 0U); configASSERT(race_done != NULL);
#endif
    printf("USB port TINYUSB_PORT_FULL_SPEED_0=%u\n", TINYUSB_PORT_FULL_SPEED_0);
    command("install");
    char line[64]; size_t used = 0;
    for (;;) {
        char c;
        if (usb_serial_jtag_read_bytes(&c, 1U, portMAX_DELAY) != 1) { continue; }
        if (c == '\n' || c == '\r') {
            line[used] = '\0'; if (used != 0U) { command(line); } used = 0;
        } else if (used + 1U < sizeof line) { line[used++] = c; }
    }
}
