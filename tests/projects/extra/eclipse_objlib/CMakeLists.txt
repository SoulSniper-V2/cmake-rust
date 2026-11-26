cmake_minimum_required(VERSION 3.20)
project(EclipseObjLib C)
# Objects of a linked OBJECT library are filed in the consumer directory's
# reserved "Object Libraries" source group (cmMakefile::AddTargetObject),
# for every target's group copy.
add_library(objs OBJECT a.c)
add_subdirectory(sub)
add_library(direct STATIC b.c $<TARGET_OBJECTS:objs>)
# GetIncludeDirectories strips implicit include directories; they come back
# only with the compiler's own list at the end of the .cproject entries.
target_include_directories(direct BEFORE PRIVATE /usr/include)
