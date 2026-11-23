cmake_minimum_required(VERSION 3.31)
project(darwin_framework_nmc C)
add_library(fw SHARED lib.c pub.h rsrc.txt)
set_target_properties(fw PROPERTIES
  FRAMEWORK TRUE
  PUBLIC_HEADER pub.h
  RESOURCE rsrc.txt
  DEBUG_POSTFIX _debug
  XCODE_ATTRIBUTE_CODE_SIGNING_REQUIRED "NO")
