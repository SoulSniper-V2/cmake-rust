add_library(linked STATIC ../b.c)
target_link_libraries(linked PRIVATE objs)
