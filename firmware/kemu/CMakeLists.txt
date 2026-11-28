# kemu: what esp-emu does not model of the Kode Dot yet, made virtual under
# unchanged kode-sdk drivers: the USB device port (TinyUSB's controller
# driver, with the page as the host), the console on UART1 and the RTC's
# time. The board's I2C chips and pins, the sound (I2S), the panel
# (MIPI-DSI), the microSD (SDMMC) and the C5 (ESP-Hosted over SDIO) are
# esp-emu's since 1.1.0, answered by kode-emu. Each piece here goes when
# esp-emu models it.
#
# WHOLE_ARCHIVE because nothing in the firmware calls into kemu by name: it
# is reached only through the --wrap'd symbols below, and the linker would
# otherwise drop it.
idf_component_register(
    SRCS "src/kemu_frame.c" "src/kemu_link.c" "src/kemu_usb.c" "src/kemu_console.c" "src/kemu_clock.c"
         "models/usbdcd.c"
    INCLUDE_DIRS "include" "models"
    REQUIRES kbsp khal
    PRIV_REQUIRES kaos kqp esp_driver_uart esp_timer esp_rom freertos
    WHOLE_ARCHIVE)

# Every symbol kemu stands in for. A driver's call to one of these lands in
# __wrap_<sym> here; nothing else changes.
set(KEMU_WRAPS
    app_main
    tinyusb_driver_install usb_new_phy usb_del_phy
    dcd_init dcd_deinit dcd_int_handler dcd_int_enable dcd_int_disable dcd_set_address
    dcd_remote_wakeup dcd_connect dcd_disconnect dcd_sof_enable dcd_edpt0_status_complete
    dcd_edpt_open dcd_edpt_close dcd_edpt_close_all dcd_edpt_iso_alloc dcd_edpt_iso_activate
    dcd_edpt_xfer dcd_edpt_xfer_fifo dcd_edpt_stall dcd_edpt_clear_stall
    usb_serial_jtag_read_bytes)

foreach(sym ${KEMU_WRAPS})
    target_link_libraries(${COMPONENT_LIB} INTERFACE "-Wl,--wrap=${sym}")
endforeach()
