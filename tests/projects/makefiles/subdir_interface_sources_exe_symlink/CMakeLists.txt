cmake_minimum_required(VERSION 3.20)
project(mkis C)
# Subdirectory Makefiles have no convenience rules for INTERFACE libraries
# (even with sources), the root Makefile does; a versioned executable's
# symlink rule has no commands; OBJECT_DEPENDS in the build tree is written
# relative to the top binary directory.
add_subdirectory(sub)
