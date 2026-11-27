project(cb_sub C)
add_library(sublib STATIC sub.c)
add_executable(subapp submain.c)
target_link_libraries(subapp PRIVATE sublib)
