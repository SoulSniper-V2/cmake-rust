#----------------------------------------------------------------
# Generated CMake target import file.
#----------------------------------------------------------------

# Commands may need to know the format version.
set(CMAKE_IMPORT_FILE_VERSION 1)

# Import target "Demo::api" for configuration ""
set_property(TARGET Demo::api APPEND PROPERTY IMPORTED_CONFIGURATIONS NOCONFIG)
set_target_properties(Demo::api PROPERTIES
  IMPORTED_LINK_INTERFACE_LANGUAGES_NOCONFIG "C"
  IMPORTED_LOCATION_NOCONFIG "${_IMPORT_PREFIX}/lib/libapi.a"
  )

list(APPEND _cmake_import_check_targets Demo::api )
list(APPEND _cmake_import_check_files_for_Demo::api "${_IMPORT_PREFIX}/lib/libapi.a" )

# Commands beyond this point should not need to know the version.
set(CMAKE_IMPORT_FILE_VERSION)
