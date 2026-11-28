# Included by kode-emu into a *copy* of the project's top CMakeLists.txt,
# right before its project() call - after everything the project set itself.
#
# Appends, never sets: kodeOS sets EXTRA_COMPONENT_DIRS and the examples set
# SDKCONFIG_DEFAULTS, and both must survive. A project that leaves
# SDKCONFIG_DEFAULTS alone gets ESP-IDF's default, sdkconfig.defaults next to
# it, which is spelled out here so that appending does not replace it.
list(APPEND EXTRA_COMPONENT_DIRS "${CMAKE_CURRENT_LIST_DIR}/kemu")

if(NOT DEFINED SDKCONFIG_DEFAULTS AND EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/sdkconfig.defaults")
    set(SDKCONFIG_DEFAULTS "${CMAKE_CURRENT_SOURCE_DIR}/sdkconfig.defaults")
endif()
list(APPEND SDKCONFIG_DEFAULTS "${CMAKE_CURRENT_LIST_DIR}/sdkconfig.emu")

# Every component found is built and linked unless a project narrows the list.
if(DEFINED COMPONENTS)
    list(APPEND COMPONENTS kemu)
endif()
