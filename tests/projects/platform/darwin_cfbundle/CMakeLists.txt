cmake_minimum_required(VERSION 3.31)
project(darwin_cfbundle C)
add_library(cf MODULE lib.c)
set_target_properties(cf PROPERTIES
  BUNDLE TRUE
  BUNDLE_EXTENSION cfbundle
  CFBUNDLE_PLIST "${CMAKE_CURRENT_SOURCE_DIR}/cf.plist.in")
