cmake_minimum_required(VERSION 4.1)
set(CMAKE_IGNORE_PATH "${CMAKE_CURRENT_SOURCE_DIR}/data/prefix/lib/cmake/acme")
    find_package(Acme CONFIG PATHS data/prefix NO_DEFAULT_PATH)

message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
