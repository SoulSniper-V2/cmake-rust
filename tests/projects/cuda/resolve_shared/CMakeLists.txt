cmake_minimum_required(VERSION 3.26)
project(resolve LANGUAGES CUDA)
add_library(shared SHARED lib.cu)
set_target_properties(shared PROPERTIES CUDA_SEPARABLE_COMPILATION ON CUDA_RESOLVE_DEVICE_SYMBOLS ON)
