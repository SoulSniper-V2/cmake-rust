include_directories(${CMAKE_CURRENT_SOURCE_DIR}/dir-only)
add_library(cfg INTERFACE)
target_include_directories(cfg INTERFACE ${CMAKE_CURRENT_BINARY_DIR})
