cmake_minimum_required(VERSION 4.1)
project(Autogen CXX)

set(QT_VERSION_MAJOR 6)
set(QT_VERSION_MINOR 5)
set(CMAKE_AUTOMOC ON)
set(CMAKE_AUTOUIC ON)
set(CMAKE_AUTORCC ON)
set(CMAKE_AUTOMOC_EXECUTABLE ${CMAKE_CURRENT_SOURCE_DIR}/tools/moc)
set(CMAKE_AUTOUIC_EXECUTABLE ${CMAKE_CURRENT_SOURCE_DIR}/tools/uic)
set(CMAKE_AUTORCC_EXECUTABLE ${CMAKE_CURRENT_SOURCE_DIR}/tools/rcc)
set(CMAKE_AUTOGEN_PARALLEL 2)
set_property(GLOBAL PROPERTY USE_FOLDERS ON)
set_property(GLOBAL PROPERTY AUTOGEN_TARGETS_FOLDER gen)
set_property(GLOBAL PROPERTY AUTOMOC_SOURCE_GROUP "gen/moc")

add_custom_command(OUTPUT gen.h
  COMMAND ${CMAKE_COMMAND} -E copy ${CMAKE_CURRENT_SOURCE_DIR}/gen.h.in gen.h
  DEPENDS gen.h.in)
add_custom_command(OUTPUT gen.qrc
  COMMAND ${CMAKE_COMMAND} -E copy ${CMAKE_CURRENT_SOURCE_DIR}/res.qrc gen.qrc
  COMMAND ${CMAKE_COMMAND} -E copy ${CMAKE_CURRENT_SOURCE_DIR}/a.txt a.txt
  COMMAND ${CMAKE_COMMAND} -E copy ${CMAKE_CURRENT_SOURCE_DIR}/res/b.bin b.bin)
add_custom_target(extra)

add_library(iface INTERFACE)
set_property(TARGET iface PROPERTY INTERFACE_AUTOMOC_MACRO_NAMES "IFACE_OBJECT;$<$<CONFIG:Debug>:DEBUG_OBJECT>")
set_property(TARGET iface PROPERTY INTERFACE_AUTOUIC_OPTIONS "-tr;ki18n")
target_compile_definitions(iface INTERFACE FROM_IFACE=1)
target_include_directories(iface SYSTEM INTERFACE ${CMAKE_CURRENT_SOURCE_DIR}/sub)
add_library(base STATIC base.cpp)
target_link_libraries(base PUBLIC iface)

add_executable(app main.cpp widget.cpp widget.h plain.hh ${CMAKE_CURRENT_BINARY_DIR}/gen.h
  res.qrc res/res.qrc ${CMAKE_CURRENT_BINARY_DIR}/gen.qrc ui/form.ui skipped.h)
target_link_libraries(app PRIVATE base)
target_include_directories(app PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/sub/../res /usr/include)
target_compile_definitions(app PRIVATE ZED "QUOTED=\"a b\"" $<$<CONFIG:Debug>:DBG>)
target_compile_features(app PRIVATE cxx_std_20)
set_target_properties(app PROPERTIES
  AUTOGEN_ORIGIN_DEPENDS ON
  AUTOGEN_TARGET_DEPENDS "extra;${CMAKE_CURRENT_SOURCE_DIR}/res/b.bin"
  AUTOMOC_MOC_OPTIONS "-nw;-Dx"
  AUTOMOC_MACRO_NAMES "MY_OBJECT;MY_OBJECT;Q_OBJECT"
  AUTOMOC_DEPEND_FILTERS "KEY;[\n][ \t]*KEY[ \t]*\"([^\"]+)\""
  AUTOMOC_PATH_PREFIX ON
  AUTOUIC_SEARCH_PATHS "ui;${CMAKE_CURRENT_SOURCE_DIR}/res/"
  AUTORCC_OPTIONS "--compress;9"
  COMPILE_WARNING_AS_ERROR OFF)
set_property(SOURCE res.qrc PROPERTY AUTORCC_OPTIONS "-name;custom;-threshold;1")
set_property(SOURCE ui/form.ui PROPERTY AUTOUIC_OPTIONS "-g;cpp")
set_property(SOURCE skipped.h PROPERTY SKIP_AUTOMOC ON)
set_property(SOURCE ui/skip.ui PROPERTY SKIP_AUTOUIC ON)
set_property(SOURCE ui/extra.ui PROPERTY AUTOUIC_OPTIONS "")

# Only AUTORCC; a custom build directory.
add_library(rcconly STATIC base.cpp res.qrc)
set_target_properties(rcconly PROPERTIES AUTOMOC OFF AUTOUIC OFF
  AUTOGEN_BUILD_DIR ${CMAKE_CURRENT_BINARY_DIR}/custom)
set_target_properties(rcconly PROPERTIES EXPORT_COMPILE_COMMANDS ON)
add_subdirectory(sub)
