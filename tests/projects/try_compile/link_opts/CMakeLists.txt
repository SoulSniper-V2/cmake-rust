cmake_minimum_required(VERSION 3.20)
project(TC C)
try_compile(OK SOURCE_FROM_CONTENT ok.c "int main(void){return 0;}\n"
  LINK_OPTIONS -Wl,--as-needed)
message(STATUS "result=${OK}")
