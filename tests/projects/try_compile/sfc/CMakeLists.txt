cmake_minimum_required(VERSION 3.20)
project(TC C)
try_compile(OK SOURCE_FROM_CONTENT genmain.c "int main(void){return 0;}\n")
message(STATUS "result=${OK}")
