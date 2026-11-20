# CMake generated Testfile for 
# Source directory: @SOURCE@
# Build directory: @BINARY@
# 
# This file includes the relevant testing commands required for 
# testing this directory and lists subdirectories to be tested as well.
add_test([=[test]=] "echo" "hello")
set_tests_properties([=[test]=] PROPERTIES  LABELS "one;two" TIMEOUT "42" WORKING_DIRECTORY "tmp" _BACKTRACE_TRIPLES "@SOURCE@/CMakeLists.txt;4;add_test;@SOURCE@/CMakeLists.txt;0;")
