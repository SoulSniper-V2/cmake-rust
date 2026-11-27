cmake_minimum_required(VERSION 3.20)
project(cc C)
# The codemodel has one configuration (with its own target and directory
# objects) per CMAKE_CONFIGURATION_TYPES entry.
set(CMAKE_CONFIGURATION_TYPES "Debug;Release;Custom")
add_library(lib STATIC l.c)
target_compile_definitions(lib PRIVATE $<$<CONFIG:Debug>:DBG> $<$<CONFIG:Custom>:CUSTOM>)
set_target_properties(lib PROPERTIES OUTPUT_NAME_RELEASE librel)
add_subdirectory(sub)
install(TARGETS lib ARCHIVE DESTINATION lib/$<CONFIG>)
