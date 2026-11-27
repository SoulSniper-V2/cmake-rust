cmake_minimum_required(VERSION 3.20)
project(cb_make C)

enable_testing()
include_directories(${CMAKE_CURRENT_SOURCE_DIR}/inc)

source_group("Core\\Sources" FILES core.c)
source_group("Core\\Headers" FILES inc/app.h)

add_library(objs OBJECT obj.c)
add_library(core STATIC core.c inc/app.h $<TARGET_OBJECTS:objs>)
target_compile_definitions(core PRIVATE CORE_VER=1)

add_executable(app main.c)
target_link_libraries(app PRIVATE core)

add_executable(optional_tool EXCLUDE_FROM_ALL opt.c)
add_custom_target(doc_stamp COMMAND ${CMAKE_COMMAND} -E echo "stamp")

add_subdirectory(sub)
add_subdirectory(excl EXCLUDE_FROM_ALL)

install(TARGETS app RUNTIME DESTINATION bin)
add_test(NAME smoke COMMAND app)
