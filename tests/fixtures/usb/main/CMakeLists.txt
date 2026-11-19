idf_component_register(SRCS "usb_main.c" INCLUDE_DIRS "."
    REQUIRES kode esp_tinyusb PRIV_REQUIRES esp_driver_usb_serial_jtag esp_hw_support esp_partition wear_levelling fatfs)
