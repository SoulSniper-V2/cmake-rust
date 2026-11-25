# cmGeneratedFileStream writes a temporary, then a rename onto a directory
# fails silently: file(CONFIGURE) succeeds and the directory stays.
set(w "${CMAKE_CURRENT_BINARY_DIR}/configure-onto-directory")
file(REMOVE_RECURSE "${w}")
file(MAKE_DIRECTORY "${w}/out")
file(CONFIGURE OUTPUT "${w}/out" CONTENT "x")
if(IS_DIRECTORY "${w}/out")
  message("still a directory")
endif()
file(WRITE "${w}/afile" "x")
file(CONFIGURE OUTPUT "${w}/afile/sub" CONTENT "x")
message("not reached")
