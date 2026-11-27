cmake_minimum_required(VERSION 3.20)
project(ec_make C)

enable_testing()
include_directories(${CMAKE_CURRENT_SOURCE_DIR}/inc)

source_group("Core\\Impl" FILES core.c)
source_group("Core\\Headers" FILES inc/ec.h)

add_library(objs OBJECT obj.c)
add_library(core STATIC core.c inc/ec.h $<TARGET_OBJECTS:objs>)
target_compile_definitions(core PRIVATE EC_DEF=1)

add_executable(app main.c)
target_link_libraries(app PRIVATE core)

add_executable(opt_tool EXCLUDE_FROM_ALL opt.c)
add_custom_target(gen_step COMMAND ${CMAKE_COMMAND} -E echo "ec")

add_subdirectory(sub)
add_subdirectory(excl EXCLUDE_FROM_ALL)

install(TARGETS app RUNTIME DESTINATION bin)
add_test(NAME ec_test COMMAND app)
