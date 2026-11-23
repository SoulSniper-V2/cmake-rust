add_executable(subthrough ../main.c)
target_link_libraries(subthrough PRIVATE dl iface)
add_executable(submark ../main.c $<TARGET_OBJECTS:p>)
target_link_libraries(submark PRIVATE iface)
