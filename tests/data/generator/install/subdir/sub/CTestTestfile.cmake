# CMake generated Testfile for 
# Source directory: @SOURCE@/sub
# Build directory: @BINARY@/sub
# 
# This file includes the relevant testing commands required for 
# testing this directory and lists subdirectories to be tested as well.
add_test([=[child]=] "echo" "child")
set_tests_properties([=[child]=] PROPERTIES  _BACKTRACE_TRIPLES "@SOURCE@/sub/CMakeLists.txt;2;add_test;@SOURCE@/sub/CMakeLists.txt;0;")
