add_executable(subexe ../main.c)
target_link_libraries(subexe PRIVATE stat shared)
set_target_properties(subexe PROPERTIES RUNTIME_OUTPUT_DIRECTORY_RELEASE ${CMAKE_BINARY_DIR}/rel-bin)
add_test(NAME subruns COMMAND subexe)
