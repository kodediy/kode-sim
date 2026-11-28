# kemu: what esp-emu does not model of the Kode Dot yet, made virtual under
# unchanged kode-sdk drivers: the console on UART1 and the RTC's time. The
# board's I2C chips and pins, the sound (I2S), the panel (MIPI-DSI), the
# microSD (SDMMC) and the C5 (ESP-Hosted over SDIO) are esp-emu's since
# 1.1.0, and the USB-C port since 1.3.0 (its host over co-simulation),
# answered by kode-emu. Each piece here goes when esp-emu models it.
#
# WHOLE_ARCHIVE because nothing in the firmware calls into kemu by name: it
# is reached only through the --wrap'd symbols below, and the linker would
# otherwise drop it.
idf_component_register(
    SRCS "src/kemu_frame.c" "src/kemu_link.c" "src/kemu_console.c" "src/kemu_clock.c"
    INCLUDE_DIRS "include"
    REQUIRES kbsp khal
    PRIV_REQUIRES kaos kqp esp_driver_uart esp_timer esp_rom freertos
    WHOLE_ARCHIVE)

# Every symbol kemu stands in for. A driver's call to one of these lands in
# __wrap_<sym> here; nothing else changes.
set(KEMU_WRAPS
    app_main
    usb_serial_jtag_read_bytes)

foreach(sym ${KEMU_WRAPS})
    target_link_libraries(${COMPONENT_LIB} INTERFACE "-Wl,--wrap=${sym}")
endforeach()
