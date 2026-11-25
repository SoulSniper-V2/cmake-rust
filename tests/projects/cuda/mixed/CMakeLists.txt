cmake_minimum_required(VERSION 3.26)
project(mixed LANGUAGES CXX CUDA)
add_executable(mixed main.cpp kernel.cu)
