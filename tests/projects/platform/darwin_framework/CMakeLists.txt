cmake_minimum_required(VERSION 3.31)
project(darwin_framework C)
add_library(fw SHARED lib.c pub.h priv.h rsrc.txt)
set_target_properties(fw PROPERTIES
  FRAMEWORK TRUE
  FRAMEWORK_VERSION B
  PUBLIC_HEADER pub.h
  PRIVATE_HEADER priv.h
  RESOURCE rsrc.txt
  XCODE_ATTRIBUTE_CODE_SIGNING_REQUIRED "NO")
