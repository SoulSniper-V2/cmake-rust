cmake_minimum_required(VERSION 4.1)
set(CMAKE_FIND_DEBUG_MODE TRUE)
find_package(Acme MODULE QUIET)
message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
