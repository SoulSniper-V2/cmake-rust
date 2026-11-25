cmake_minimum_required(VERSION 3.26)
project(standard17 LANGUAGES CUDA)
add_executable(standard17 main.cu)
set_target_properties(standard17 PROPERTIES CUDA_STANDARD 17 CUDA_STANDARD_REQUIRED ON)
