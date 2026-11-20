# CMake generated Testfile for 
# Source directory: @SOURCE@
# Build directory: @BINARY@
# 
# This file includes the relevant testing commands required for 
# testing this directory and lists subdirectories to be tested as well.
add_test([=[legacy]=] "cmd" "a b" "a\"b" "a\b")
set_tests_properties([=[legacy]=] PROPERTIES  _BACKTRACE_TRIPLES "@SOURCE@/CMakeLists.txt;4;add_test;@SOURCE@/CMakeLists.txt;0;")
