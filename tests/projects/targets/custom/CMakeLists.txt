cmake_minimum_required(VERSION 4.1)
project(x NONE)

add_custom_command(OUTPUT main.out secondary.out
  COMMAND "${CMAKE_COMMAND}" -E touch main.out secondary.out
  BYPRODUCTS side.out
  DEPENDS "${CMAKE_CURRENT_LIST_FILE}"
  MAIN_DEPENDENCY "${CMAKE_CURRENT_LIST_FILE}"
  DEPFILE main.d
  JOB_POOL example
  JOB_SERVER_AWARE ON
  WORKING_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}"
  COMMENT "Generating outputs"
  COMMAND_EXPAND_LISTS DEPENDS_EXPLICIT_ONLY VERBATIM CODEGEN)
add_custom_command(OUTPUT secondary.out APPEND
  COMMAND "${CMAKE_COMMAND}" -E echo append)
set_property(GLOBAL PROPERTY JOB_POOLS example=1)
add_custom_target(work ALL DEPENDS main.out
  COMMAND "${CMAKE_COMMAND}" -E touch target.out
  BYPRODUCTS target.out SOURCES CMakeLists.txt)
add_custom_command(TARGET work POST_BUILD
  COMMAND "${CMAKE_COMMAND}" -E echo post BYPRODUCTS post.out)
add_custom_target(hidden)

enable_testing()
add_test(old "${CMAKE_COMMAND}" -E echo old)
add_test(old "${CMAKE_COMMAND}" -E echo replacement)
add_test(NAME "new test" COMMAND "${CMAKE_COMMAND}" -E echo "a;b"
  CONFIGURATIONS Debug Release WORKING_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}"
  COMMAND_EXPAND_LISTS)
get_property(test_names DIRECTORY PROPERTY TESTS)
get_property(test_dir TEST "new test" PROPERTY WORKING_DIRECTORY)
get_target_property(work_sources work SOURCES)
get_target_property(hidden_default hidden EXCLUDE_FROM_ALL)
message("tests=[${test_names}]")
message("test directory=[${test_dir}]")
message("sources=[${work_sources}] excluded=[${hidden_default}]")
foreach(file IN ITEMS main.out secondary.out side.out target.out post.out)
  get_source_file_property(generated "${CMAKE_CURRENT_BINARY_DIR}/${file}" GENERATED)
  message("${file}.generated=[${generated}]")
endforeach()
