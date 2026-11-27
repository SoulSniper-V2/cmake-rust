cmake_minimum_required(VERSION 3.20)
project(cl_make C)

enable_testing()
include_directories(${CMAKE_CURRENT_SOURCE_DIR}/inc)

source_group("Core\\Impl" FILES core.c)
source_group("Core\\Headers" FILES inc/util.h)

add_library(objs OBJECT obj.c)
add_library(core STATIC core.c inc/util.h $<TARGET_OBJECTS:objs>)

add_executable(app main.c)
target_link_libraries(app PRIVATE core)

add_executable(opt_tool EXCLUDE_FROM_ALL opt.c)
add_custom_target(cl_custom COMMAND ${CMAKE_COMMAND} -E echo "codelite")

add_subdirectory(sub)
add_subdirectory(excl EXCLUDE_FROM_ALL)

install(TARGETS app RUNTIME DESTINATION bin)
add_test(NAME smoke COMMAND app)
