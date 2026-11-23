cmake_minimum_required(VERSION 3.31)
project(darwin_app C)
add_executable(app MACOSX_BUNDLE main.c res.txt data.dat pub.h priv.h)
set_target_properties(app PROPERTIES
  MACOSX_BUNDLE_INFO_STRING "A bundle"
  MACOSX_BUNDLE_BUNDLE_NAME "App"
  MACOSX_BUNDLE_BUNDLE_VERSION "2.5"
  MACOSX_BUNDLE_GUI_IDENTIFIER "org.example.app"
  MACOSX_BUNDLE_SHORT_VERSION_STRING "2.5"
  MACOSX_BUNDLE_LONG_VERSION_STRING "2.5.0"
  MACOSX_BUNDLE_COPYRIGHT "none"
  RESOURCE "data.dat")
set_source_files_properties(res.txt PROPERTIES
  MACOSX_PACKAGE_LOCATION Resources/text)
set_source_files_properties(pub.h PROPERTIES PUBLIC_HEADER ON)
set_source_files_properties(priv.h PROPERTIES PRIVATE_HEADER ON)
