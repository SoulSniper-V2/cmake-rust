add_library(objects OBJECT parent.f90 child.f90)
add_library(wrapped STATIC $<TARGET_OBJECTS:objects>)
target_include_directories(wrapped INTERFACE "${CMAKE_CURRENT_BINARY_DIR}")
export(TARGETS wrapped NAMESPACE F:: FILE wrapped.cmake)
