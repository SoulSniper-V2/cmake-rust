# CMake generated Testfile for 
# Source directory: @SOURCE@
# Build directory: @BINARY@
# 
# This file includes the relevant testing commands required for 
# testing this directory and lists subdirectories to be tested as well.
set(CTEST_RESOURCE_SPEC_FILE "resources.json")
include("a.cmake")
include("b.cmake")
include("c.cmake")
set_directory_properties(PROPERTIES LABELS "local";"global")
