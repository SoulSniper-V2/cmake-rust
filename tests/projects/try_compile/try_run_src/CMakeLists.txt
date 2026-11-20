cmake_minimum_required(VERSION 3.20)
project(TC C)
try_run(RUN_RES COMPILE_RES SOURCE_FROM_CONTENT r.c
  "int main(void){return 0;}\n" RUN_OUTPUT_VARIABLE RUROUT COMPILE_OUTPUT_VARIABLE COUT)
message(STATUS "compile=${COMPILE_RES} run=${RUN_RES}")
