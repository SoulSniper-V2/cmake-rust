cmake_minimum_required(VERSION 4.1)
project(x NONE)

add_compile_options(-directory-first)
add_definitions(-DOLD -DKEEP=two -fexample)
remove_definitions(-DOLD -fexample)
include_directories(old-include)
link_directories(old-link)
add_link_options(-directory-link)
link_libraries(optimized inherited)

add_library(include_only INTERFACE)
add_library(obj OBJECT CMakeLists.txt)
set_property(TARGET obj PROPERTY LINKER_LANGUAGE C)
add_custom_target(worker)
add_custom_target(alpha)
add_custom_target(zed)
add_dependencies(obj worker zed alpha worker)

add_compile_options(-directory-last)
add_compile_definitions(-DAFTER=1)
include_directories(BEFORE SYSTEM new-include another-include)
link_directories(new-link)
add_link_options(-directory-link-last)

target_include_directories(obj SYSTEM BEFORE PUBLIC "public dir" "$<BUILD_INTERFACE:/gen>")
target_compile_definitions(obj PUBLIC -DFOO=1 PRIVATE BAR)
target_compile_options(obj PUBLIC -public)
target_compile_options(obj BEFORE PRIVATE -first)
target_compile_features(obj PRIVATE cxx_std_17)
target_link_options(obj BEFORE PUBLIC -public-link)
target_link_directories(obj BEFORE PRIVATE relative)
target_precompile_headers(obj PUBLIC "<vector>" some-header.h)
target_link_libraries(obj PRIVATE debug include_only -lpthread PUBLIC other)
target_sources(obj PUBLIC FILE_SET HEADERS FILES CMakeLists.txt
                          FILE_SET header_set TYPE HEADERS BASE_DIRS . FILES CMakeLists.txt)
target_sources(include_only INTERFACE "$<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/CMakeLists.txt>")

foreach(prop IN ITEMS INCLUDE_DIRECTORIES INTERFACE_INCLUDE_DIRECTORIES
    INTERFACE_SYSTEM_INCLUDE_DIRECTORIES COMPILE_OPTIONS INTERFACE_COMPILE_OPTIONS
    COMPILE_DEFINITIONS INTERFACE_COMPILE_DEFINITIONS COMPILE_FEATURES LINK_DIRECTORIES
    LINK_OPTIONS LINK_LIBRARIES INTERFACE_LINK_LIBRARIES PRECOMPILE_HEADERS
    HEADER_SETS INTERFACE_HEADER_SETS HEADER_SET HEADER_DIRS HEADER_SET_header_set
    MANUALLY_ADDED_DEPENDENCIES SOURCES)
  get_property(value TARGET obj PROPERTY "${prop}")
  string(REPLACE "${CMAKE_CURRENT_SOURCE_DIR}" "<SOURCE>" value "${value}")
  message("${prop}=[${value}]")
endforeach()
get_property(definitions DIRECTORY PROPERTY COMPILE_DEFINITIONS)
message("directory definitions=[${definitions}]")
