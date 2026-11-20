# CMake generated Testfile for 
# Source directory: @SOURCE@
# Build directory: @BINARY@
# 
# This file includes the relevant testing commands required for 
# testing this directory and lists subdirectories to be tested as well.
if(CTEST_CONFIGURATION_TYPE MATCHES "^([Rr][Ee][Ll][Ee][Aa][Ss][Ee]|[Dd][Ee][Bb][Uu][Gg])$")
  add_test([=[test]=] "echo")
  set_tests_properties([=[test]=] PROPERTIES  _BACKTRACE_TRIPLES "@SOURCE@/CMakeLists.txt;4;add_test;@SOURCE@/CMakeLists.txt;0;")
endif()
