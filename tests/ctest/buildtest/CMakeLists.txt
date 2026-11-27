cmake_minimum_required(VERSION 3.16)
project(BuildAndTestFixture NONE)
file(COPY "${CMAKE_CURRENT_SOURCE_DIR}/bt-src" DESTINATION "${CMAKE_CURRENT_BINARY_DIR}")
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/bt-file" "not a directory\n")
