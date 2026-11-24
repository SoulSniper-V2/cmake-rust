# Sibling binary directory: subdirs() gets "../bdir", as for googlemock.
add_subdirectory(../b "${CMAKE_BINARY_DIR}/bdir")
add_subdirectory(../c "${CMAKE_CURRENT_BINARY_DIR}/nested/c")
add_test(NAME ta COMMAND ${CMAKE_COMMAND} -E true)
