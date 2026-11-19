# kemu: what esp-emu does not model of the Kode Dot yet, made virtual under
# unchanged kode-sdk drivers: the USB device port (TinyUSB's controller
# driver, with the page as the host), the console on UART1 and the RTC's
# time. The board's I2C chips and pins, the sound (I2S), the panel
# (MIPI-DSI), the microSD (SDMMC) and the C5 (ESP-Hosted over SDIO) are
# esp-emu's since 1.1.0, answered by kode-sim. Each piece here goes when
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
    usb_new_phy usb_del_phy
    dcd_init dcd_deinit dcd_int_handler dcd_int_enable dcd_int_disable dcd_set_address
    dcd_remote_wakeup dcd_connect dcd_disconnect dcd_sof_enable dcd_edpt0_status_complete
    dcd_edpt_open dcd_edpt_close dcd_edpt_close_all dcd_edpt_iso_alloc dcd_edpt_iso_activate
    dcd_edpt_xfer dcd_edpt_xfer_fifo dcd_edpt_stall dcd_edpt_clear_stall
    usb_serial_jtag_read_bytes)

foreach(sym ${KEMU_WRAPS})
    target_link_libraries(${COMPONENT_LIB} INTERFACE "-Wl,--wrap=${sym}")
endforeach()

# Use the actual TinyUSB ABI and the exact config its component compiles with.
# Do not add TinyUSB as a dependency of applications which do not use USB.
idf_build_get_property(kemu_components BUILD_COMPONENTS)
foreach(candidate tinyusb espressif__tinyusb)
    if(candidate IN_LIST kemu_components)
        idf_component_get_property(kemu_tusb_lib ${candidate} COMPONENT_LIB)
        break()
    endif()
endforeach()
if(kemu_tusb_lib)
    target_compile_definitions(${COMPONENT_LIB} PRIVATE KEMU_USB_TINYUSB=1)
    target_include_directories(${COMPONENT_LIB} PRIVATE
        "$<TARGET_PROPERTY:${kemu_tusb_lib},INCLUDE_DIRECTORIES>")
    target_link_libraries(${COMPONENT_LIB} PRIVATE ${kemu_tusb_lib})

    # Unlike kemu, this archive must not be WHOLE_ARCHIVE: the ESP-TinyUSB
    # member is needed only when that API is used, never by direct users.
    add_library(kemu_usb_lifecycle STATIC src/kemu_tusb_lifecycle.c src/kemu_usb_task_delete.c)
    target_include_directories(kemu_usb_lifecycle PRIVATE include
        "$<TARGET_PROPERTY:${kemu_tusb_lib},INCLUDE_DIRECTORIES>")
    target_link_libraries(kemu_usb_lifecycle PRIVATE ${kemu_tusb_lib} idf::esp_hw_support)
    set(kemu_usb_lifecycle_libs kemu_usb_lifecycle ${kemu_tusb_lib})
    foreach(sym tusb_rhport_init tud_rhport_init tusb_deinit tud_deinit tud_task_ext vTaskDelete xQueueGenericSend)
        target_link_libraries(${COMPONENT_LIB} INTERFACE "-Wl,--wrap=${sym}")
    endforeach()

    # ESP-TinyUSB defines the port identifier; direct P4 TinyUSB documents
    # the same convention in common/tusb_mcu.h (port 0 FS, port 1 HS).
    foreach(candidate esp_tinyusb espressif__esp_tinyusb)
        if(candidate IN_LIST kemu_components)
            idf_component_get_property(kemu_esp_tusb_lib ${candidate} COMPONENT_LIB)
            idf_component_get_property(kemu_esp_tusb_dir ${candidate} COMPONENT_DIR)
            target_sources(kemu_usb_lifecycle PRIVATE src/kemu_esp_tinyusb.c)
            target_include_directories(kemu_usb_lifecycle PRIVATE "${kemu_esp_tusb_dir}/include")
            target_include_directories(${COMPONENT_LIB} PRIVATE "${kemu_esp_tusb_dir}/include")
            target_link_libraries(kemu_usb_lifecycle PRIVATE ${kemu_esp_tusb_lib})
            list(APPEND kemu_usb_lifecycle_libs ${kemu_esp_tusb_lib})
            foreach(sym tinyusb_driver_install tinyusb_driver_uninstall)
                target_link_libraries(${COMPONENT_LIB} INTERFACE "-Wl,--wrap=${sym}")
            endforeach()
            target_compile_definitions(${COMPONENT_LIB} PRIVATE KEMU_USB_ESP_TINYUSB=1)
            target_compile_definitions(kemu_usb_lifecycle PRIVATE KEMU_USB_ESP_TINYUSB=1)
            break()
        endif()
    endforeach()
    # --wrap introduces references that CMake cannot see in its dependency
    # graph. Rescan these archives when ESP's task reaches the stack wrappers.
    target_link_libraries(${COMPONENT_LIB} INTERFACE
        "-Wl,--start-group" ${kemu_usb_lifecycle_libs} "-Wl,--end-group")
endif()
