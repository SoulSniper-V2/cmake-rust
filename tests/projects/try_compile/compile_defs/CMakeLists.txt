cmake_minimum_required(VERSION 3.20)
project(TC C)
try_compile(OK SOURCE_FROM_CONTENT ok.c "int main(void){return 0;}\n"
  COMPILE_DEFINITIONS FOO=1 -DBAR=xx BAZ)
message(STATUS "result=${OK}")
