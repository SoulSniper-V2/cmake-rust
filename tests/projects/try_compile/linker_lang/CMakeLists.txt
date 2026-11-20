cmake_minimum_required(VERSION 3.20)
project(TC CXX)
try_compile(OK SOURCES ${CMAKE_CURRENT_SOURCE_DIR}/main.cxx LINKER_LANGUAGE CXX)
file(WRITE ${CMAKE_CURRENT_SOURCE_DIR}/main.cxx "int main(){return 0;}\n")
