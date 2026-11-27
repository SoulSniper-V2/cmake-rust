cmake_minimum_required(VERSION 3.20)
project(clt_make C)

enable_testing()
source_group("Core" FILES core.c inc/clt.h)
include_directories(${CMAKE_CURRENT_SOURCE_DIR}/inc)

add_library(objs OBJECT obj.c)
add_library(core STATIC core.c inc/clt.h $<TARGET_OBJECTS:objs>)
add_executable(app main.c)
target_link_libraries(app PRIVATE core)

add_executable(opt_tool EXCLUDE_FROM_ALL opt.c)
add_custom_target(clt_custom COMMAND ${CMAKE_COMMAND} -E echo "clt")

add_subdirectory(sub)
add_subdirectory(excl EXCLUDE_FROM_ALL)

install(TARGETS app RUNTIME DESTINATION bin)
add_test(NAME smoke COMMAND app)
